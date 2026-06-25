use std::{sync::Arc, time::Duration};

use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
    utils::command::BotCommands,
};

use tokio::sync::Mutex;

use crate::{
    boil::BoilClient,
    config::{save_cron, validate_cron, Config},
    core::{check_ip_quality, do_reconnect},
    timer::TimerManager,
};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "命令列表:")]
enum Command {
    #[command(description = "开始使用")]
    Start,
    #[command(description = "查看当前 IP 和今日剩余次数")]
    Status,
    #[command(description = "检查当前 IP 质量")]
    Check,
    #[command(description = "换 IP（重拨）")]
    Change,
    #[command(description = "设置定时换 IP，如 /timer 0 */6 * * * 或 /timer off")]
    Timer(String),
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let token = config
        .tg_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("未配置 TG_TOKEN，请运行 boil setup"))?;

    let bot = Bot::new(token);
    bot.set_my_commands(Command::bot_commands()).await?;

    let config = Arc::new(config);

    // 定时器管理器：共享给命令处理器，实现 /timer 运行时热更新（无需重启进程）
    let timer = Arc::new(Mutex::new(TimerManager::new(config.clone()).await?));
    if let Some(cron) = &config.change_cron {
        if let Err(e) = timer.lock().await.set(cron).await {
            log::error!("定时器启动失败: {e}");
        }
    }
    let _monitor = crate::monitor::start(config.clone());

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config, timer])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    config: Arc<Config>,
    timer: Arc<Mutex<TimerManager>>,
) -> ResponseResult<()> {
    let chat_id_str = msg.chat.id.to_string();
    if config.tg_chat_id.as_deref() != Some(&chat_id_str) {
        return Ok(());
    }
    match cmd {
        Command::Start => {
            bot.send_message(
                msg.chat.id,
                "👋 <b>Redial Bot</b>\n\n/status — 查看当前 IP 和今日剩余次数\n/change — 换 IP（重拨）",
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Command::Status => tg_status(&bot, msg.chat.id, &config).await,
        Command::Check => tg_check(&bot, msg.chat.id, &config).await,
        Command::Change => tg_change(&bot, msg.chat.id, &config).await,
        Command::Timer(arg) => tg_timer(&bot, msg.chat.id, &timer, arg.trim()).await,
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<Config>,
) -> ResponseResult<()> {
    let uid = q.from.id.to_string();
    if config.tg_chat_id.as_deref() != Some(&uid) {
        bot.answer_callback_query(&q.id).await?;
        return Ok(());
    }
    bot.answer_callback_query(&q.id).await?;

    let chat_id = match &q.message {
        Some(msg) => msg.chat.id,
        None => return Ok(()),
    };

    if let Some(data) = &q.data {
        if let Some(rest) = data.strip_prefix("change:") {
            let mut parts = rest.splitn(2, ':');
            if let (Some(router_id), Some(interface)) = (parts.next(), parts.next()) {
                tg_do_reconnect(&bot, chat_id, &config, router_id, interface, None).await;
            }
        }
    }
    Ok(())
}

async fn tg_status(bot: &Bot, chat_id: ChatId, config: &Config) {
    let result = async {
        let c = BoilClient::new()?;
        c.query_all_authed(&config.boil_account, &config.boil_password).await
    }
    .await;

    match result {
        Ok(data) => {
            let mut lines = vec![format!(
                "📡 <b>服务器状态</b> | 今日换 IP {}/{} 次\n",
                data.daily_used, data.daily_limit
            )];
            for item in &data.zone_items {
                let ip = data.get_ip(&item.router_id, &item.interface).unwrap_or("未知");
                let tag = if item.nat_no_change { "🔒 NAT" } else { "✅ 可换" };
                lines.push(format!("{}\n<code>{}</code> | {}", item.label, ip, tag));
            }
            let _ = bot
                .send_message(chat_id, lines.join("\n"))
                .parse_mode(ParseMode::Html)
                .await;
        }
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("❌ 查询失败: {e}")).await;
        }
    }
}

async fn tg_check(bot: &Bot, chat_id: ChatId, config: &Config) {
    let result = async {
        let c = BoilClient::new()?;
        c.query_all_authed(&config.boil_account, &config.boil_password).await
    }
    .await;

    let data = match result {
        Ok(d) => d,
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("❌ 查询失败: {e}")).await;
            return;
        }
    };

    let changeable = data.changeable();
    if changeable.is_empty() {
        let _ = bot.send_message(chat_id, "⚠️ 没有可检测的服务器").await;
        return;
    }

    let _ = bot.send_message(chat_id, "🔍 检测中，请稍候...").await;

    let mut lines = Vec::new();
    for r in &changeable {
        let ip = match data.get_ip(&r.router_id, &r.interface) {
            Some(ip) => ip.to_string(),
            None => continue,
        };
        if let Some(q) = check_ip_quality(&ip).await {
            lines.push(format!(
                "📍 <b>{}</b>\nIP: <code>{}</code>\n地区: {} | ISP: {}\n类型: {} | CF 风险: {}",
                r.label, ip, q.country, q.isp, q.ip_type(), q.cf_risk()
            ));
        }
    }

    // 流媒体检测：仅在本机 IP 与 Boil VPS IP 一致时才有意义
    // 用全部服务器（含 NAT 不可换）的 IP 比对，否则本机若是 NAT 机会被误判
    let boil_ips: Vec<String> = data.zone_items
        .iter()
        .filter_map(|r| data.get_ip(&r.router_id, &r.interface))
        .map(str::to_string)
        .collect();
    let local_ip = get_local_public_ip().await;
    let on_boil_vps = local_ip.as_deref()
        .map(|ip| boil_ips.iter().any(|b| b == ip))
        .unwrap_or(false);

    if on_boil_vps {
        let streaming = crate::streaming::check_all().await;
        let streaming_lines: Vec<String> = streaming
            .iter()
            .map(|r| format!("  {:16} {}", r.service, r.status.display()))
            .collect();
        lines.push(format!(
            "\n📺 <b>流媒体解锁</b>\n<pre>{}</pre>",
            streaming_lines.join("\n")
        ));
    } else {
        lines.push("\n📺 <b>流媒体检测</b>\n运行于非 Boil VPS，跳过（结果无意义）".to_string());
    }

    let _ = bot
        .send_message(chat_id, lines.join("\n\n"))
        .parse_mode(ParseMode::Html)
        .await;
}

async fn tg_timer(bot: &Bot, chat_id: ChatId, timer: &Arc<Mutex<TimerManager>>, arg: &str) {
    // 无参数：显示当前设置（从运行中的调度器读，保证与实际生效状态一致）
    if arg.is_empty() {
        let current = timer.lock().await.current();
        let msg = match current {
            Some(cron) => format!("⏰ 当前定时换 IP: <code>{cron}</code>\n\n关闭: /timer off\n修改示例:\n  每6小时: /timer 0 */6 * * *\n  每天3点: /timer 0 3 * * *"),
            None => "⏰ 定时换 IP 未启用\n\n设置示例:\n  每6小时: /timer 0 */6 * * *\n  每天3点: /timer 0 3 * * *".to_string(),
        };
        let _ = bot.send_message(chat_id, msg).parse_mode(ParseMode::Html).await;
        return;
    }

    // off：关闭定时（先持久化，再热更新运行中的调度器）
    if arg.eq_ignore_ascii_case("off") {
        if let Err(e) = save_cron(None) {
            let _ = bot.send_message(chat_id, format!("❌ 保存失败: {e}")).await;
            return;
        }
        match timer.lock().await.clear().await {
            Ok(_) => { let _ = bot.send_message(chat_id, "✅ 定时换 IP 已关闭，立即生效").await; }
            Err(e) => { let _ = bot.send_message(chat_id, format!("⚠️ 已写入配置，但热更新失败，重启后生效: {e}")).await; }
        }
        return;
    }

    // 验证 → 持久化 → 热更新运行中的调度器
    if let Err(e) = validate_cron(arg) {
        let _ = bot.send_message(chat_id, format!("❌ {e}\n\n示例:\n  每6小时: 0 */6 * * *\n  每天3点: 0 3 * * *")).await;
        return;
    }
    if let Err(e) = save_cron(Some(arg)) {
        let _ = bot.send_message(chat_id, format!("❌ 保存失败: {e}")).await;
        return;
    }
    match timer.lock().await.set(arg).await {
        Ok(_) => {
            let _ = bot.send_message(
                chat_id,
                format!("✅ 定时换 IP 已设置: <code>{arg}</code>，立即生效"),
            )
            .parse_mode(ParseMode::Html)
            .await;
        }
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("⚠️ 已写入配置，但热更新失败，重启后生效: {e}")).await;
        }
    }
}

async fn tg_change(bot: &Bot, chat_id: ChatId, config: &Config) {
    let result = async {
        let c = BoilClient::new()?;
        c.query_all_authed(&config.boil_account, &config.boil_password).await
    }
    .await;

    let data = match result {
        Ok(d) => d,
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("❌ 登录失败: {e}")).await;
            return;
        }
    };

    let changeable = data.changeable();
    if changeable.is_empty() {
        let _ = bot.send_message(chat_id, "⚠️ 没有可换 IP 的服务器").await;
        return;
    }

    if changeable.len() == 1 {
        let r = changeable[0];
        let (router_id, interface) = (r.router_id.clone(), r.interface.clone());
        drop(changeable);
        tg_do_reconnect(bot, chat_id, config, &router_id, &interface, Some(data)).await;
        return;
    }

    let buttons: Vec<Vec<InlineKeyboardButton>> = changeable
        .iter()
        .map(|r| {
            vec![InlineKeyboardButton::callback(
                r.label.clone(),
                format!("change:{}:{}", r.router_id, r.interface),
            )]
        })
        .collect();

    let _ = bot
        .send_message(chat_id, "选择要换 IP 的服务器：")
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn tg_do_reconnect(
    bot: &Bot,
    chat_id: ChatId,
    config: &Config,
    router_id: &str,
    interface: &str,
    pre_data: Option<crate::boil::QueryAllResponse>,
) {
    let _ = bot.send_message(chat_id, "⏳ 开始换 IP，请稍候...").await;

    match do_reconnect(config, router_id, interface, pre_data).await {
        Ok(res) => match res.new_ip {
            Some(new_ip) => {
                let reach = if res.reachable { "TCP 可达 ✅" } else { "TCP 未通 ⚠️" };
                let quality_line = match &res.quality {
                    Some(q) => format!(
                        "\n\n📊 <b>IP 质量</b>\n地区: {}\nISP: {}\n类型: {}\nCF 风险: {}",
                        q.country, q.isp, q.ip_type(), q.cf_risk()
                    ),
                    None => String::new(),
                };
                let _ = bot
                    .send_message(
                        chat_id,
                        format!(
                            "✅ <b>换 IP 完成</b>\n旧 IP: <code>{}</code>\n新 IP: <code>{new_ip}</code> <i>{reach}</i>{quality_line}",
                            res.old_ip.as_deref().unwrap_or("未知"),
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            None => {
                let _ = bot
                    .send_message(
                        chat_id,
                        format!(
                            "⚠️ 重拨已触发，但未检测到 IP 变化\n旧 IP: <code>{}</code>\n请到面板手动确认",
                            res.old_ip.as_deref().unwrap_or("未知"),
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
        },
        Err(e) => {
            let _ = bot.send_message(chat_id, format!("❌ 换 IP 失败: {e}")).await;
        }
    }
}

async fn get_local_public_ip() -> Option<String> {
    let client = reqwest::Client::new();
    // 多源兜底：单一服务超时/被墙时仍能拿到本机公网 IP，避免误判为非 VPS
    for url in ["https://api.ipify.org", "https://ifconfig.me/ip", "https://icanhazip.com"] {
        if let Ok(resp) = client.get(url).timeout(Duration::from_secs(5)).send().await {
            if let Ok(text) = resp.text().await {
                let ip = text.trim().to_string();
                if !ip.is_empty() {
                    return Some(ip);
                }
            }
        }
    }
    None
}
