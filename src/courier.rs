//! 赛博信使：物流轨迹模拟。
//!
//! 本地账户之间投递邮件后，服务器会在收件人目录下创建物流任务：
//!
//! ```text
//! <程序目录>/<收件人>/from-<寄件人>-<UTC 时间>.json        轨迹本体（JSONL）
//! <程序目录>/<收件人>/from-<寄件人>-<UTC 时间>.meta.json   任务元数据与投递进度
//! ```
//!
//! 后台每分钟轮询一次所有任务：
//! 1. `pending` 的任务读取账户下的 `deepseek-api-key`，调用 LLM 生成 12-16 条物流轨迹；
//! 2. `ready` 的任务按轨迹里的 UTC 时间把到期条目以邮件形式同时投递到收件人与寄件人邮箱。
//!
//! 没有密钥或调用失败时不会降级伪造内容，只会把任务标记为 `failed` 并记录原因。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::accounts::Accounts;
use crate::mailstore::MailStore;

/// 生成轨迹时使用的提示词模板，`{{...}}` 为待填充的任务变量。
const PROMPT_TEMPLATE: &str = r#"你现在是一个“赛博信使物流轨迹模拟器”。请根据用户提供的任务变量，模拟一封信从起点到终点的完整物流信息，并只输出 JSONL。

【任务变量】
当前时间（UTC）：{{当前时间}}
起点：{{起点}}
终点：{{终点}}
寄件人：{{寄件人}}
收件人：{{收件人}}
总时长：{{总时长}}
特殊要求：{{特殊要求}}
信件摘要：{{信件摘要}}

【路线要求】
1. 起点与终点都是现实中的地点，请按两地实际地理距离规划路线：跨省需经过真实的分拨中心、
   集散中心或航空口岸，同城直接进入区域分拨与派送。
2. 各环节耗时要与距离匹配：同城几小时，省内半天到一天，跨省一到数天，跨国数天到一周。
3. 你可以自行判断起终点距离量级，不要出现明显不合理的传送。

【信件影响】
信件摘要是这封信的实际内容。请自行判断其中是否有影响物流时效与方式的因素，例如：
- 药品、生鲜、生物样本 → 冷链、温控、保温箱、优先中转；
- 易碎品、陶瓷、玻璃 → 防压、加固、人工分拣；
- 证件、银行卡、现金 → 安检审查、专人押运、身份核验；
- 急件、限时达、今天就要 → 加急通道、优先派送；
- 体积大或超重 → 改用陆运、大件专线。
有就体现在对应的检查与中转环节里，没有就按普通信件处理。
轨迹本身仍然是物流流水账，不要把信件内容原文写进 message，也不要把它当成小说情节展开。

【输出规则】
1. 只输出 JSONL，每行一个独立、合法的 JSON 对象。
2. 不要输出数组、Markdown 表格、解释文字、标题或空行。
3. 每行 JSON 只能包含以下四个字段，字段名必须完全一致：
   {"date":"", "courier-name":"", "message":"", "offset-minutes":0}
4. “offset-minutes”是相对揽收时刻的分钟数，必须是递增的非负整数，第一条为 0，
   最后一条不得超前总时长。“date”字段留空字符串即可，服务端会自行换算成真实时间。
5. “负责人的英文”使用随机英文人名，可重复但不要连续大量重复。
6. “做什么事情”必须使用物流、快递、分拣、安检、中转、派送、签收等官方术语。
7. 允许加入搞怪异常事件，但必须同时包含异常登记、人工复核、路线纠偏或异常处理等官方流程描述。
8. 不得写成文艺故事或小说，不要抒情，不要对白，不要心理描写。
9. 路线可以故意绕远，但必须体现分拨、中转、错分、回流、派送等物流环节。
10. 最后一条必须包含签收、上传签收凭证、关闭追踪码等状态。
11. 总条数建议 12-16 条；如果用户指定条数，以用户指定为准。
12. 输出前自检：每行是否合法 JSON；字段是否为四个；offset-minutes 是否递增；是否从揽收到签收；是否包含绕路和搞怪异常；是否只输出 JSONL。

【开始生成】
请根据以上变量和规则，直接输出 JSONL，不要输出任何额外说明。"#;

/// 传给 LLM 的信件正文最大字符数。
const LETTER_LIMIT: usize = 600;

/// 物流任务的运行配置。
pub struct CourierConfig {
    /// 程序目录（`accounts.toml` 所在目录），任务与用户资料都放在这里。
    pub workspace: PathBuf,
    /// 服务端域名，用于生成地址兜底值。
    pub domain: String,
    /// 模拟总时长（分钟），会作为「总时长」交给 LLM。
    pub span_minutes: u32,
    /// 轮询间隔，默认 60 秒。
    pub interval: Duration,
    /// LLM 接口地址。
    pub endpoint: String,
    /// LLM 模型名。
    pub model: String,
    /// 投递物流通知时使用的邮箱存储。
    pub store: MailStore,
}

/// 任务元数据，保存在 `.meta.json` 里。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    /// 寄件人账户。
    pub sender: String,
    /// 收件人账户。
    pub recipient: String,
    /// 创建时间（Unix 秒）。
    pub created_at: i64,
    /// 模拟总时长（分钟）。
    pub span_minutes: u32,
    /// 起点描述（来自寄件人的 `{账户}.md`）。
    pub origin: String,
    /// 终点描述（来自收件人的 `{账户}.md`）。
    pub destination: String,
    /// 特殊要求（沿用邮件主题）。
    pub special: String,
    /// 信件正文摘要，用于让轨迹与信件内容呼应（旧任务可能没有该字段）。
    #[serde(default)]
    pub letter: String,
    /// 任务状态：pending / ready / done / failed。
    pub status: String,
    /// 已投递的轨迹条数。
    pub sent: usize,
    /// 轨迹总条数。
    pub total: usize,
    /// 失败原因。
    #[serde(default)]
    pub error: Option<String>,
}

/// 轨迹中的一条记录，字段名与输出规格保持一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    /// 时间，格式 `YYYY-MM-DD HH:MM`（UTC），由服务端按 `offset_minutes` 换算。
    /// 模型只需要给 `offset-minutes`，这里允许缺省。
    #[serde(default)]
    pub date: String,
    /// 负责人的英文名。
    #[serde(rename = "courier-name")]
    pub courier_name: String,
    /// 物流状态描述。
    pub message: String,
    /// 相对揽收时刻的分钟数，模型只给偏移量，时间由服务端填。
    /// 只在生成阶段使用，写入轨迹文件时省略。
    #[serde(
        rename = "offset-minutes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub offset_minutes: Option<u32>,
}

/// 创建一条物流任务，供 SMTP 投递成功后调用。
pub async fn create_task(
    cfg: &CourierConfig,
    sender: &str,
    recipient: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let now = chrono::Utc::now();
    // 带毫秒，避免同一秒内多封邮件互相覆盖。
    let stamp = now.format("%Y%m%d-%H%M%S%.3f").to_string();
    let dir = cfg.workspace.join(recipient);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("创建任务目录失败: {}", dir.display()))?;

    let base = format!("from-{sender}-{stamp}");
    let meta = TaskMeta {
        sender: sender.to_string(),
        recipient: recipient.to_string(),
        created_at: now.timestamp(),
        span_minutes: cfg.span_minutes,
        origin: read_location(&cfg.workspace, sender, &cfg.domain),
        destination: read_location(&cfg.workspace, recipient, &cfg.domain),
        special: if subject.trim().is_empty() {
            "无".to_string()
        } else {
            format!("邮件主题：{subject}")
        },
        letter: summarize_letter(body),
        status: "pending".to_string(),
        sent: 0,
        total: 0,
        error: None,
    };

    write_meta(&dir, &base, &meta).await?;
    tokio::fs::write(dir.join(format!("{base}.json")), b"")
        .await
        .with_context(|| format!("创建轨迹文件失败: {}", base))?;
    println!("[courier] 已创建物流任务 {base}（{sender} → {recipient}）");
    Ok(())
}

/// 启动后台轮询，按分钟检查所有任务。
pub async fn run(cfg: Arc<CourierConfig>, accounts: Arc<Accounts>) {
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(err) = poll_once(&cfg, &accounts).await {
            eprintln!("[courier] 轮询出错: {err}");
        }
    }
}

/// 扫描所有账户目录，推进各自的任务。
async fn poll_once(cfg: &CourierConfig, accounts: &Accounts) -> Result<()> {
    for name in accounts.map.keys() {
        let dir = cfg.workspace.join(name);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            // 账户还没有任何任务目录，跳过即可。
            Err(_) => continue,
        };

        let mut metas: Vec<(String, PathBuf)> = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if let Some(base) = file_name.strip_suffix(".meta.json") {
                if base.starts_with("from-") {
                    metas.push((base.to_string(), entry.path()));
                }
            }
        }
        metas.sort_by(|left, right| left.0.cmp(&right.0));

        for (base, meta_path) in metas {
            if let Err(err) = process_task(cfg, &dir, &base, &meta_path).await {
                eprintln!("[courier] 处理任务 {base} 失败: {err}");
            }
        }
    }
    Ok(())
}

/// 处理单个任务：未生成的去生成，已生成的投递到期条目。
async fn process_task(
    cfg: &CourierConfig,
    dir: &Path,
    base: &str,
    meta_path: &Path,
) -> Result<()> {
    let text = tokio::fs::read_to_string(meta_path).await?;
    let mut meta: TaskMeta = serde_json::from_str(&text)
        .with_context(|| format!("任务元数据不是合法 JSON: {}", meta_path.display()))?;
    let json_path = dir.join(format!("{base}.json"));

    match meta.status.as_str() {
        "pending" => generate(cfg, &mut meta, meta_path, &json_path, base).await,
        "ready" => dispatch(cfg, &mut meta, meta_path, &json_path).await,
        _ => Ok(()),
    }
}

/// 调用 LLM 生成轨迹并写入任务文件。
async fn generate(
    cfg: &CourierConfig,
    meta: &mut TaskMeta,
    meta_path: &Path,
    json_path: &Path,
    base: &str,
) -> Result<()> {
    let Some(key) = load_api_key(&cfg.workspace, &meta.recipient) else {
        meta.status = "failed".to_string();
        meta.error = Some("缺少 deepseek-api-key，已跳过物流轨迹生成".to_string());
        save_meta(meta_path, meta).await?;
        eprintln!(
            "[courier] {base}: 账户 {} 缺少 deepseek-api-key，任务标记失败",
            meta.recipient
        );
        return Ok(());
    };

    // 以生成时刻作为揽收基准，模型只给偏移量，作息由服务端掌控。
    let base_time = chrono::Utc::now().naive_utc();
    let prompt = build_prompt(meta, base_time);
    match request_trace(cfg, &key, &prompt, base_time, meta.span_minutes).await {
        Ok(entries) if !entries.is_empty() => {
            let mut content = String::new();
            for entry in &entries {
                content.push_str(&serde_json::to_string(entry)?);
                content.push('\n');
            }
            tokio::fs::write(json_path, content)
                .await
                .with_context(|| format!("写入轨迹文件失败: {}", json_path.display()))?;
            meta.status = "ready".to_string();
            meta.total = entries.len();
            meta.sent = 0;
            meta.error = None;
            save_meta(meta_path, meta).await?;
            println!("[courier] {base}: 已生成 {} 条物流轨迹", entries.len());
        }
        Ok(_) => {
            meta.status = "failed".to_string();
            meta.error = Some("LLM 没有返回可用的轨迹行".to_string());
            save_meta(meta_path, meta).await?;
            eprintln!("[courier] {base}: LLM 返回内容无法解析");
        }
        Err(err) => {
            meta.status = "failed".to_string();
            meta.error = Some(format!("{err:#}"));
            save_meta(meta_path, meta).await?;
            eprintln!("[courier] {base}: 生成轨迹失败: {err:#}");
        }
    }
    Ok(())
}

/// 把已经到期的轨迹条目投递出去。
async fn dispatch(
    cfg: &CourierConfig,
    meta: &mut TaskMeta,
    meta_path: &Path,
    json_path: &Path,
) -> Result<()> {
    let content = tokio::fs::read_to_string(json_path).await?;
    let entries: Vec<TraceEntry> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if entries.is_empty() {
        meta.status = "failed".to_string();
        meta.error = Some("轨迹文件为空".to_string());
        save_meta(meta_path, meta).await?;
        return Ok(());
    }

    let now = chrono::Utc::now().naive_utc();
    let mut changed = false;
    while meta.sent < entries.len() {
        let entry = &entries[meta.sent];
        let due = NaiveDateTime::parse_from_str(&entry.date, "%Y-%m-%d %H:%M").ok();
        match due {
            Some(time) if time <= now => {
                deliver_update(cfg, meta, entry, meta.sent + 1, entries.len()).await;
                meta.sent += 1;
                changed = true;
            }
            // 还没到时间，后面的条目更晚，直接等下一轮。
            _ => break,
        }
    }

    if meta.sent >= entries.len() {
        meta.status = "done".to_string();
        changed = true;
    }
    if changed {
        save_meta(meta_path, meta).await?;
    }
    Ok(())
}

/// 把一条轨迹同时投递到收件人和寄件人的邮箱。
async fn deliver_update(
    cfg: &CourierConfig,
    meta: &TaskMeta,
    entry: &TraceEntry,
    index: usize,
    total: usize,
) {
    let mail = build_update_mail(cfg, meta, entry, index, total);
    let mut targets = vec![meta.recipient.clone(), meta.sender.clone()];
    targets.dedup();
    for target in targets {
        match cfg.store.deliver(&target, &mail).await {
            Ok(uid) => println!(
                "[courier] 物流通知 {index}/{total} 已投递到 {target}（编号 {uid}）"
            ),
            Err(err) => eprintln!("[courier] 投递到 {target} 失败: {err}"),
        }
    }
}

/// 组装一条物流通知邮件。
fn build_update_mail(
    cfg: &CourierConfig,
    meta: &TaskMeta,
    entry: &TraceEntry,
    index: usize,
    total: usize,
) -> Vec<u8> {
    let body = format!(
        "运单：{}-{}\n起点：{}\n终点：{}\n时间：{} (UTC)\n负责人：{}\n进度：{}/{}\n\n{}\n",
        meta.sender,
        meta.created_at,
        meta.origin,
        meta.destination,
        entry.date,
        entry.courier_name,
        index,
        total,
        entry.message
    );

    let subject = format!(
        "物流更新 {index}/{total}：{} → {}（{}）",
        meta.origin, meta.destination, entry.date
    );
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S +0000")
        .to_string();

    let mut mail = Vec::with_capacity(body.len() * 2 + 256);
    mail.extend_from_slice(
        format!(
            "From: {} <courier@{}>\r\n",
            encode_header("赛博信使"),
            cfg.domain
        )
        .as_bytes(),
    );
    mail.extend_from_slice(format!("To: <{}@{}>\r\n", meta.recipient, cfg.domain).as_bytes());
    mail.extend_from_slice(format!("Subject: {}\r\n", encode_header(&subject)).as_bytes());
    mail.extend_from_slice(format!("Date: {date}\r\n").as_bytes());
    mail.extend_from_slice(
        format!(
            "Message-ID: <courier.{}.{}@{}>\r\n",
            meta.created_at, index, cfg.domain
        )
        .as_bytes(),
    );
    mail.extend_from_slice(b"MIME-Version: 1.0\r\n");
    mail.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
    mail.extend_from_slice(b"Content-Transfer-Encoding: base64\r\n");
    mail.extend_from_slice(b"\r\n");
    mail.extend_from_slice(base64_wrapped(body.as_bytes()).as_bytes());
    mail
}

/// 按 76 字符一行做 Base64 编码，符合 MIME 要求。
fn base64_wrapped(data: &[u8]) -> String {
    let encoded = STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 2);
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push_str("\r\n");
    }
    out
}

/// 把非 ASCII 邮件头编码成 RFC 2047 的 Base64 形式。
fn encode_header(value: &str) -> String {
    format!("=?UTF-8?B?{}?=", STANDARD.encode(value.as_bytes()))
}

/// 调用 LLM 并解析返回的 JSONL。
///
/// `base_time` 是揽收时刻，用来把模型给出的 `offset-minutes` 换算成真实时间。
async fn request_trace(
    cfg: &CourierConfig,
    key: &str,
    prompt: &str,
    base_time: NaiveDateTime,
    span_minutes: u32,
) -> Result<Vec<TraceEntry>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("创建 HTTP 客户端失败")?;
    let payload = serde_json::json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 1.0,
        // deepseek-flash 默认开启思考，思考与正式回答共用输出额度，
        // 额度太小会把 JSONL 截断，这里留足空间。
        "max_tokens": 8192,
        "stream": false,
    });

    let response = client
        .post(&cfg.endpoint)
        .bearer_auth(key)
        .json(&payload)
        .send()
        .await
        .context("调用 LLM 接口失败")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("LLM 返回 {status}: {}", truncate(&text, 200));
    }

    let value: serde_json::Value =
        serde_json::from_str(&text).context("LLM 返回的不是合法 JSON")?;
    let message = value.pointer("/choices/0/message");
    let content = message
        .and_then(|item| item.get("content"))
        .and_then(|item| item.as_str())
        .unwrap_or_default();
    let entries = parse_trace(content, base_time, span_minutes);
    if !entries.is_empty() {
        return Ok(entries);
    }

    // 开启思考的模型（如 deepseek-flash 默认行为）如果额度耗尽，
    // content 会是空的，此时从 reasoning_content 里找 JSONL 兑底。
    let reasoning = message
        .and_then(|item| item.get("reasoning_content"))
        .and_then(|item| item.as_str())
        .unwrap_or_default();
    let entries = parse_trace(reasoning, base_time, span_minutes);
    if entries.is_empty() {
        bail!("LLM 返回里没有可用的轨迹行（content 为空或无法解析）");
    }
    Ok(entries)
}

/// 逐行解析 LLM 返回的 JSONL，按基准时间把偏移量换算成真实时间。
///
/// `base` 是揽收时刻（UTC），模型只负责给出相对分钟数，不负责写日期，
/// 这样不会因为模型自己编年份而生成过期轨迹。
/// `span_minutes` 是总时长，超出范围的条目会被丢弃。
fn parse_trace(content: &str, base: NaiveDateTime, span_minutes: u32) -> Vec<TraceEntry> {
    let mut entries: Vec<TraceEntry> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        let line = line.strip_prefix("```json").unwrap_or(line);
        let line = line.strip_prefix("```").unwrap_or(line).trim();
        if line.is_empty() {
            continue;
        }
        let Ok(mut entry) = serde_json::from_str::<TraceEntry>(line) else {
            continue;
        };
        if entry.message.trim().is_empty() {
            continue;
        }

        // 优先用 offset-minutes；兼容旧格式里直接给了合法 date 的情况。
        let date = match entry.offset_minutes {
            Some(minutes) if minutes <= span_minutes => {
                base + chrono::Duration::minutes(i64::from(minutes))
            }
            Some(_) => continue,
            None => match NaiveDateTime::parse_from_str(&entry.date, "%Y-%m-%d %H:%M") {
                Ok(parsed) => parsed,
                Err(_) => continue,
            },
        };

        // 时间必须严格递增。
        if let Some(last) = entries.last() {
            let Ok(last_at) = NaiveDateTime::parse_from_str(&last.date, "%Y-%m-%d %H:%M") else {
                continue;
            };
            if date <= last_at {
                continue;
            }
        }

        entry.date = date.format("%Y-%m-%d %H:%M").to_string();
        // 偏移量只在生成阶段使用，写入文件后保持三个字段的既定格式。
        entry.offset_minutes = None;
        entries.push(entry);
    }
    entries
}

/// 把邮件正文压成传给 LLM 的摘要：合并空白并限制长度，避免提示词过长。
fn summarize_letter(body: &str) -> String {
    let mut text = String::new();
    let mut count = 0usize;
    let mut last_space = false;
    for ch in body.trim().chars() {
        if ch.is_whitespace() {
            // 连续空白（含换行）压成单个空格。
            if !last_space && !text.is_empty() {
                text.push(' ');
                last_space = true;
            }
            continue;
        }
        if count >= LETTER_LIMIT {
            text.push('…');
            break;
        }
        text.push(ch);
        count += 1;
        last_space = false;
    }
    text.trim_end().to_string()
}

/// 用任务变量填充提示词模板。
///
/// `now` 是生成时刻（UTC），告诉模型“现在几点”，避免它把轨迹写到过去的年份。
fn build_prompt(meta: &TaskMeta, now: NaiveDateTime) -> String {
    let letter = if meta.letter.trim().is_empty() {
        "（无正文）".to_string()
    } else {
        // 正文里如果出现 `{{...}}`，会被后面的 replace 当成新变量，先打散。
        meta.letter.replace("{{", "{ {")
    };
    PROMPT_TEMPLATE
        .replace("{{当前时间}}", &now.format("%Y-%m-%d %H:%M").to_string())
        .replace("{{起点}}", &meta.origin)
        .replace("{{终点}}", &meta.destination)
        .replace("{{寄件人}}", &meta.sender)
        .replace("{{收件人}}", &meta.recipient)
        .replace("{{总时长}}", &format!("{} 分钟", meta.span_minutes))
        .replace("{{特殊要求}}", &meta.special)
        .replace("{{信件摘要}}", &letter)
}

/// 读取账户的 API 密钥：优先账户目录，其次程序目录。
fn load_api_key(workspace: &Path, account: &str) -> Option<String> {
    let candidates = [
        workspace.join(account).join("deepseek-api-key"),
        workspace.join("deepseek-api-key"),
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let key = text.trim();
            if !key.is_empty() {
                return Some(key.to_string());
            }
        }
    }
    None
}

/// 从 `{账户}.md` 中读取地址信息，读不到时退回 `账户@域名`。
fn read_location(workspace: &Path, account: &str, domain: &str) -> String {
    let path = workspace.join(format!("{account}.md"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(location) = extract_location(&text) {
            return truncate(&location, 120);
        }
    }
    format!("{account}@{domain}")
}

/// 在个人资料里找「地址：xxx」这类行；没有标签时接受一行像地点的短文本。
fn extract_location(text: &str) -> Option<String> {
    const LABELS: [&str; 6] = ["地址", "位置", "所在地", "城市", "住址", "地点"];

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for label in LABELS {
            if let Some(rest) = line.strip_prefix(label) {
                let rest = rest.trim_start_matches([':', '：', ' ', '\t']).trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
        let lowered = line.to_ascii_lowercase();
        for label in ["address", "location", "city"] {
            if let Some(rest) = lowered.strip_prefix(label) {
                let rest = rest.trim_start_matches([':', ' ', '\t']).trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }

    // 没有标签时，接受第一行“看起来像地名”的短句（排除自我介绍）。
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.len() > 24 {
            continue;
        }
        if line.contains(['。', '！', '!', '？', '?']) {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

/// 写入任务元数据。
async fn write_meta(dir: &Path, base: &str, meta: &TaskMeta) -> Result<()> {
    let path = dir.join(format!("{base}.meta.json"));
    let text = serde_json::to_vec_pretty(meta)?;
    tokio::fs::write(&path, text)
        .await
        .with_context(|| format!("写入任务元数据失败: {}", path.display()))
}

/// 覆盖保存任务元数据。
async fn save_meta(path: &Path, meta: &TaskMeta) -> Result<()> {
    let text = serde_json::to_vec_pretty(meta)?;
    tokio::fs::write(path, text)
        .await
        .with_context(|| format!("保存任务元数据失败: {}", path.display()))
}

/// 按字符数截断，避免日志和提示词过长。
fn truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的固定“当前时间”，避免断言依赖真实时钟。
    fn test_now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-10-18 09:30", "%Y-%m-%d %H:%M").unwrap()
    }

    // 模板里的占位符必须被全部替换。
    #[test]
    fn prompt_substitution() {
        let meta = TaskMeta {
            sender: "alice".into(),
            recipient: "bob".into(),
            created_at: 0,
            span_minutes: 90,
            origin: "北京市朝阳区".into(),
            destination: "上海市浦东新区".into(),
            special: "无".into(),
            letter: String::new(),
            status: "pending".into(),
            sent: 0,
            total: 0,
            error: None,
        };
        let prompt = build_prompt(&meta, test_now());
        assert!(!prompt.contains("{{"));
        assert!(prompt.contains("起点：北京市朝阳区"));
        assert!(prompt.contains("总时长：90 分钟"));
        assert!(prompt.contains("信件摘要：（无正文）"));
        // 当前时间必须真的填进去，而不是留给模型自己编。
        assert!(prompt.contains("当前时间（UTC）：2026-10-18 09:30"));
    }

    // 信件正文要压掉换行并限制长度，且不能把正文里的占位符当成模板变量。
    #[test]
    fn prompt_includes_letter() {
        let meta = TaskMeta {
            sender: "alice".into(),
            recipient: "bob".into(),
            created_at: 0,
            span_minutes: 90,
            origin: "北京市".into(),
            destination: "上海市".into(),
            special: "无".into(),
            letter: summarize_letter("奶奶：\r\n\r\n药收到了吗？\n记得按时吃 {{终点}}"),
            status: "pending".into(),
            sent: 0,
            total: 0,
            error: None,
        };
        let prompt = build_prompt(&meta, test_now());
        assert!(prompt.contains("奶奶： 药收到了吗？ 记得按时吃"));
        // 正文里的 `{{终点}}` 不得被替换成真实地点。
        assert!(prompt.contains("{ {终点}}"));
        assert!(prompt.contains("终点：上海市"));
    }

    // 超长正文会被截断并补上省略号。
    #[test]
    fn letter_is_truncated() {
        let long = "字".repeat(LETTER_LIMIT + 50);
        let summary = summarize_letter(&long);
        assert!(summary.ends_with('…'));
        assert_eq!(summary.chars().count(), LETTER_LIMIT + 1);
        assert_eq!(summarize_letter("  "), "");
    }

    // 非法行、时间倒流的行都要被过滤，偏移量要换算成真实时间。
    #[test]
    fn trace_parsing() {
        let content = r#"{"date":"2025-01-01 10:00","courier-name":"Tom","message":"揽收"}
not json
{"date":"2025-01-01 09:00","courier-name":"Amy","message":"倒流"}
{"date":"2025-01-01 11:00","courier-name":"Jack","message":"签收"}
```"#;
        let entries = parse_trace(content, test_now(), 180);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].courier_name, "Tom");
        assert_eq!(entries[1].message, "签收");
    }

    // 模型只给 offset-minutes 时，时间应当由服务端按基准时间换算。
    #[test]
    fn trace_uses_offsets() {
        let content = r#"{"courier-name":"Tom","message":"揽收","offset-minutes":0}
{"courier-name":"Amy","message":"到达转运中心","offset-minutes":45}
{"courier-name":"Jack","message":"签收","offset-minutes":150}
{"courier-name":"Evil","message":"超出总时长","offset-minutes":999}"#;
        let entries = parse_trace(content, test_now(), 180);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].date, "2026-10-18 09:30");
        assert_eq!(entries[1].date, "2026-10-18 10:15");
        assert_eq!(entries[2].date, "2026-10-18 12:00");
        // 偏移量属于生成期的中间字段，写入轨迹文件后应当不出现。
        assert!(entries.iter().all(|entry| entry.offset_minutes.is_none()));
    }

    // 偏移量不递增或模型自编日期时，不能污染结果。
    #[test]
    fn trace_rejects_bad_offsets() {
        let content = r#"{"courier-name":"A","message":"一","offset-minutes":10}
{"courier-name":"B","message":"二","offset-minutes":5}
{"courier-name":"C","message":"三","offset-minutes":20}"#;
        let entries = parse_trace(content, test_now(), 180);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].date, "2026-10-18 09:40");
        assert_eq!(entries[1].date, "2026-10-18 09:50");
    }

    // 个人资料里的地址行优先，其次才是短句。
    #[test]
    fn profile_location() {
        assert_eq!(
            extract_location("# alice\n\n地址：杭州市西湖区\n喜欢写信").unwrap(),
            "杭州市西湖区"
        );
        assert_eq!(extract_location("成都市武侯区").unwrap(), "成都市武侯区");
        // 自我介绍不是地点，应当被排除，由调用方退回 `账户@域名`。
        assert!(extract_location("我是 alice，喜欢写信。").is_none());
        assert_eq!(extract_location("地址：北京市").unwrap(), "北京市");
    }

    // 邮件头编码后必须是合法的 RFC 2047 编码词。
    #[test]
    fn header_encoding() {
        let encoded = encode_header("物流更新");
        assert!(encoded.starts_with("=?UTF-8?B?"));
        assert!(encoded.ends_with("?="));
    }
}
