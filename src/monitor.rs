use std::{sync::Arc, time::Duration};

use tokio::{net::TcpStream, task::JoinHandle, time};

use crate::{boil::BoilClient, config::Config, core::do_reconnect};

const CHECK_HOST: &str = "www.189.cn";
const CHECK_PORT: u16 = 80;
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 启动 TCP 健康检查：每 5 分钟检测 www.189.cn:80，不通则触发换 IP。
pub fn start(config: Arc<Config>) -> JoinHandle<()> {
    tokio::spawn(async move {
        log::info!("TCP 健康检查已启动: {CHECK_HOST}:{CHECK_PORT}, interval=5m");
        let mut interval = time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if check_tcp().await {
                log::info!("TCP 健康检查通过: {CHECK_HOST}:{CHECK_PORT}");
                continue;
            }

            log::warn!("TCP 健康检查失败: {CHECK_HOST}:{CHECK_PORT}，准备换 IP");
            tg_notify(
                &config,
                &format!("⚠️ TCP 检测失败: {CHECK_HOST}:{CHECK_PORT}\n准备自动换 IP"),
            )
            .await;

            if let Err(e) = change_first_available(&config).await {
                log::error!("TCP 健康检查触发换 IP 失败: {e}");
                tg_notify(&config, &format!("❌ TCP 检测触发换 IP 失败: {e}")).await;
            }
        }
    })
}

async fn check_tcp() -> bool {
    time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((CHECK_HOST, CHECK_PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

async fn change_first_available(config: &Config) -> anyhow::Result<()> {
    let c = BoilClient::new()?;
    let data = c
        .query_all_authed(&config.boil_account, &config.boil_password)
        .await?;

    let local_ip = get_local_public_ip()
        .await
        .ok_or_else(|| anyhow::anyhow!("无法获取本机公网 IP，未执行换 IP"))?;

    let matched = data
        .zone_items
        .iter()
        .find(|r| data.get_ip(&r.router_id, &r.interface) == Some(local_ip.as_str()))
        .ok_or_else(|| anyhow::anyhow!("本机公网 IP {local_ip} 未匹配到面板服务器，未执行换 IP"))?;

    anyhow::ensure!(
        !matched.nat_no_change && matched.status == "ok",
        "本机对应服务器不可换 IP: {} ({local_ip})",
        matched.label
    );

    let target = (matched.router_id.clone(), matched.interface.clone());
    let target_label = matched.label.clone();

    let res = do_reconnect(config, &target.0, &target.1, Some(data)).await?;
    match res.new_ip {
        Some(new_ip) => {
            let msg = format!(
                "✅ TCP 检测触发换 IP 完成\n服务器: {}\n旧 IP: {}\n新 IP: {}",
                target_label,
                res.old_ip.as_deref().unwrap_or("未知"),
                new_ip,
            );
            log::info!("{msg}");
            tg_notify(config, &msg).await;
        }
        None => {
            let msg = format!(
                "⚠️ TCP 检测触发重拨，但未检测到 IP 变化\n服务器: {}\n旧 IP: {}",
                target_label,
                res.old_ip.as_deref().unwrap_or("未知")
            );
            log::warn!("{msg}");
            tg_notify(config, &msg).await;
        }
    }

    Ok(())
}

async fn get_local_public_ip() -> Option<String> {
    let client = reqwest::Client::new();
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

async fn tg_notify(config: &Config, msg: &str) {
    let (token, chat_id) = match (&config.tg_token, &config.tg_chat_id) {
        (Some(t), Some(c)) => (t, c),
        _ => return,
    };
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let _ = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "chat_id": chat_id, "text": msg }))
        .send()
        .await;
}
