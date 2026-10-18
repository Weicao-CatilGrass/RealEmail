//! RealEmail：极简 POP3 + SMTP 本地邮件服务端。
//!
//! 设计要点：
//! - SMTP 监听 3629，POP3 监听 3628，都可用命令行参数改写；
//! - 账户来自程序目录下的 `accounts.toml`，密码必须是 AES-256-GCM 密文；
//! - 只接收本地账户作为收件人，邮件直接投递到对应账户的本地邮箱；
//! - 发往 `system` 账户的邮件会按主题触发命令（见 `commands` 模块）。

mod accounts;
mod commands;
mod crypto;
mod mailstore;
mod pop3;
mod proto;
mod smtp;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

use accounts::Accounts;
use crypto::Cipher;
use mailstore::MailStore;

/// SMTP 默认端口。
const DEFAULT_SMTP_PORT: u16 = 3629;
/// POP3 默认端口（需要第二个端口时使用）。
const DEFAULT_POP3_PORT: u16 = 3628;
/// 服务端默认域名，仅用于问候语与地址展示。
const DEFAULT_DOMAIN: &str = "realemail.local";
/// 单封邮件默认大小上限（MB）。
const DEFAULT_MAX_MESSAGE_MB: u64 = 25;

/// 命令行参数。
#[derive(Parser, Debug)]
#[command(
    name = "realemail",
    version,
    about = "极简 POP3 + SMTP 本地邮件服务端",
    long_about = None
)]
struct Cli {
    /// 账户文件路径，默认是程序所在目录下的 accounts.toml。
    #[arg(long, global = true, value_name = "PATH")]
    accounts: Option<PathBuf>,

    /// 加密密钥文件路径，默认与账户文件同目录的 mail.key。
    #[arg(long = "key-file", global = true, value_name = "PATH")]
    key_file: Option<PathBuf>,

    /// 邮件数据目录，默认与账户文件同目录的 data。
    #[arg(long = "data-dir", global = true, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// 子命令。
#[derive(Subcommand, Debug)]
enum Command {
    /// 启动服务（默认命令）。
    Serve {
        /// 监听地址，默认 0.0.0.0。
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// SMTP 端口，默认 3629。
        #[arg(long = "smtp-port", default_value_t = DEFAULT_SMTP_PORT)]
        smtp_port: u16,
        /// POP3 端口，默认 3628。
        #[arg(long = "pop3-port", default_value_t = DEFAULT_POP3_PORT)]
        pop3_port: u16,
        /// 服务端域名，写入问候语与 Received 头。
        #[arg(long, default_value = DEFAULT_DOMAIN)]
        domain: String,
        /// 单封邮件大小上限（MB）。
        #[arg(long = "max-message-mb", default_value_t = DEFAULT_MAX_MESSAGE_MB)]
        max_message_mb: u64,
    },
    /// 新增账户并设置密码。
    Add {
        /// 账户名。
        account: String,
        /// 直接指定密码；不填则交互式输入。
        #[arg(short, long)]
        password: Option<String>,
    },
    /// 修改已有账户的密码。
    Passwd {
        /// 账户名。
        account: String,
        /// 直接指定密码；不填则交互式输入。
        #[arg(short, long)]
        password: Option<String>,
    },
    /// 把明文密码加密成可写入 accounts.toml 的密文。
    Encrypt {
        /// 明文密码。
        password: String,
    },
    /// 列出所有账户名（不显示密码）。
    List,
    /// 打印实际使用的账户、密钥、数据目录。
    Paths,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 默认路径都相对“程序所在目录”，保证双击运行和 cargo run 行为一致。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let accounts_path = cli
        .accounts
        .clone()
        .unwrap_or_else(|| exe_dir.join("accounts.toml"));
    let key_path = cli
        .key_file
        .clone()
        .unwrap_or_else(|| sibling(&accounts_path, "mail.key"));
    let data_dir = cli
        .data_dir
        .clone()
        .unwrap_or_else(|| sibling(&accounts_path, "data"));

    let command = cli.command.unwrap_or_else(default_serve);

    match command {
        Command::Serve {
            host,
            smtp_port,
            pop3_port,
            domain,
            max_message_mb,
        } => {
            serve(
                &accounts_path,
                &key_path,
                data_dir,
                host,
                smtp_port,
                pop3_port,
                domain,
                max_message_mb,
            )
            .await
        }
        Command::Paths => {
            println!("账户文件: {}", accounts_path.display());
            println!("密钥文件: {}", key_path.display());
            println!("数据目录: {}", data_dir.display());
            println!("脚本目录: {}", workspace_dir(&accounts_path).display());
            Ok(())
        }
        Command::Encrypt { password } => {
            let cipher = Cipher::load_or_create(&key_path)?;
            println!("{}", cipher.encrypt(&password)?);
            Ok(())
        }
        Command::List => {
            let cipher = Cipher::load_or_create(&key_path)?;
            let accounts = load_accounts_or_hint(&accounts_path, &cipher)?;
            println!("{} 中共有 {} 个账户：", accounts_path.display(), accounts.map.len());
            for name in accounts.map.keys() {
                println!("  {name}");
            }
            Ok(())
        }
        Command::Add { account, password } => {
            let name = accounts::normalize_account_name(&account)?;
            let cipher = Cipher::load_or_create(&key_path)?;
            let mut accounts = if accounts_path.exists() {
                Accounts::load(&accounts_path, &cipher)?
            } else {
                Accounts::empty(accounts_path.clone())
            };
            if accounts.map.contains_key(&name) {
                bail!("账户 `{name}` 已存在，请改用 `realemail passwd {name}` 修改密码");
            }
            accounts.map.insert(name.clone(), prompt_password(password)?);
            accounts.save(&cipher)?;
            println!("已添加账户 `{name}` 到 {}", accounts_path.display());
            Ok(())
        }
        Command::Passwd { account, password } => {
            let name = accounts::normalize_account_name(&account)?;
            let cipher = Cipher::load_or_create(&key_path)?;
            let mut accounts = load_accounts_or_hint(&accounts_path, &cipher)?;
            if !accounts.map.contains_key(&name) {
                bail!("账户 `{name}` 不存在，请先执行 `realemail add {name}`");
            }
            accounts.map.insert(name.clone(), prompt_password(password)?);
            accounts.save(&cipher)?;
            println!("已更新账户 `{name}` 的密码");
            Ok(())
        }
    }
}

/// 不带子命令时默认执行 serve。
fn default_serve() -> Command {
    Command::Serve {
        host: "0.0.0.0".to_string(),
        smtp_port: DEFAULT_SMTP_PORT,
        pop3_port: DEFAULT_POP3_PORT,
        domain: DEFAULT_DOMAIN.to_string(),
        max_message_mb: DEFAULT_MAX_MESSAGE_MB,
    }
}

/// 启动 SMTP 与 POP3 两个监听器。
#[allow(clippy::too_many_arguments)]
async fn serve(
    accounts_path: &Path,
    key_path: &Path,
    data_dir: PathBuf,
    host: String,
    smtp_port: u16,
    pop3_port: u16,
    domain: String,
    max_message_mb: u64,
) -> Result<()> {
    let cipher = Cipher::load_or_create(key_path)?;
    let accounts = Arc::new(load_accounts_or_hint(accounts_path, &cipher)?);
    let workspace = workspace_dir(accounts_path);

    let store = MailStore::new(data_dir.clone())?;
    for name in accounts.map.keys() {
        store.ensure_account(name)?;
    }

    println!("账户文件: {}", accounts_path.display());
    println!("密钥文件: {}", key_path.display());
    println!("数据目录: {}", store.root().display());
    println!("脚本目录: {}", workspace.display());
    println!("已加载 {} 个账户", accounts.map.len());

    let smtp_ctx = Arc::new(smtp::SmtpCtx {
        host: domain.clone(),
        accounts: accounts.clone(),
        store: store.clone(),
        workspace: workspace.clone(),
        max_message: (max_message_mb as usize) * 1024 * 1024,
    });
    let pop3_ctx = Arc::new(pop3::Pop3Ctx {
        host: domain.clone(),
        accounts: accounts.clone(),
        store: store.clone(),
        active: Default::default(),
    });

    let smtp_listener = TcpListener::bind((host.as_str(), smtp_port))
        .await
        .with_context(|| format!("无法监听 SMTP 端口 {host}:{smtp_port}"))?;
    let pop3_listener = TcpListener::bind((host.as_str(), pop3_port))
        .await
        .with_context(|| format!("无法监听 POP3 端口 {host}:{pop3_port}，可用 --pop3-port 换一个"))?;

    println!("SMTP 监听: {host}:{smtp_port}");
    println!("POP3 监听: {host}:{pop3_port}");
    if host != "127.0.0.1" && host != "localhost" {
        println!("提示：服务使用明文协议，未启用 TLS，请只在内网或本机使用");
    }

    tokio::select! {
        result = smtp::run(smtp_listener, smtp_ctx) => result,
        result = pop3::run(pop3_listener, pop3_ctx) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("收到中断信号，正在退出");
            Ok(())
        }
    }
}

/// 加载账户文件，文件不存在时给出可直接照做的提示。
fn load_accounts_or_hint(path: &Path, cipher: &Cipher) -> Result<Accounts> {
    if !path.exists() {
        bail!(
            "找不到账户文件 {}。请先添加账户，例如：realemail add alice",
            path.display()
        );
    }
    Accounts::load(path, cipher)
}

/// 与账户文件同级的路径（如 mail.key、data）。
fn sibling(path: &Path, name: &str) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// system 命令脚本的存放目录：与账户文件同目录，也就是程序所在目录。
fn workspace_dir(accounts_path: &Path) -> PathBuf {
    match accounts_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 读取密码：命令行给了就用命令行的，否则交互式输入两次。
fn prompt_password(from_args: Option<String>) -> Result<String> {
    if let Some(password) = from_args {
        if password.is_empty() {
            bail!("密码不能为空");
        }
        return Ok(password);
    }
    let first = rpassword::prompt_password("请输入密码: ").context("读取密码失败")?;
    if first.is_empty() {
        bail!("密码不能为空");
    }
    let second = rpassword::prompt_password("请再次输入密码: ").context("读取密码失败")?;
    if first != second {
        bail!("两次输入的密码不一致");
    }
    Ok(first)
}
