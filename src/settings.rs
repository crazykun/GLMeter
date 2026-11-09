//! 本地网页设置页：菜单「⚙ 设置…」起一个只绑 127.0.0.1 的微型 HTTP 服务，
//! 用浏览器打开随机令牌路径下的配置表单，替代手编 config.toml。
//!
//! 零依赖：HTTP 与表单解析手写、HTML/CSS 全部内联。安全模型：
//! 回环地址 + 128-bit 随机令牌路径即鉴权；校验 Host（防 DNS rebinding）与
//! POST 的 Origin（防跨站伪造）；请求体上限 64KB；服务 15 分钟生命周期，
//! 到期自动关闭，再次点菜单换新令牌（旧链接自然失效）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api;
use crate::config::{self, Config};
use crate::{notify, open_url, Cmd};

/// 服务生命周期：到期自动关闭，再次点菜单会起新实例（换新令牌）
const SERVER_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// 请求头与请求体大小上限
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 64 * 1024;

struct ActiveServer {
    port: u16,
    token: String,
    deadline: Instant,
    shutdown: std::sync::Arc<AtomicBool>,
}

static ACTIVE: Mutex<Option<ActiveServer>> = Mutex::new(None);

/// 打开设置页：已有存活服务则复用（同一令牌），否则起服务线程后再开浏览器
pub fn open(cmd_tx: Sender<Cmd>) {
    if let Some(url) = ensure_server(cmd_tx) {
        open_url(&url);
    }
}

/// 确保设置服务在跑并返回其 URL；绑定失败返回 None（已打日志）。
/// 独立成函数便于测试：直接拿 URL 做断言，不拉起浏览器
fn ensure_server(cmd_tx: Sender<Cmd>) -> Option<String> {
    let mut active = ACTIVE.lock().unwrap();
    if let Some(s) = active.as_ref() {
        if !s.shutdown.load(Ordering::Relaxed) && Instant::now() < s.deadline {
            return Some(format!("http://127.0.0.1:{}/{}/", s.port, s.token));
        }
    }
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[GLMeter] 设置页端口绑定失败: {e}");
            return None;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let token = api::new_request_id();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    *active = Some(ActiveServer {
        port,
        token: token.clone(),
        deadline: Instant::now() + SERVER_LIFETIME,
        shutdown: std::sync::Arc::clone(&shutdown),
    });

    let url = format!("http://127.0.0.1:{port}/{token}/");
    eprintln!("[GLMeter] 设置页已就绪: {url}");
    let _ = std::thread::Builder::new()
        .name("settings".into())
        .spawn(move || {
            serve(listener, &token, cmd_tx, shutdown);
        });
    Some(url)
}

/// 服务循环：nonblocking accept 轮询，便于及时响应关闭标志与到期时间
fn serve(
    listener: TcpListener,
    token: &str,
    cmd_tx: Sender<Cmd>,
    shutdown: std::sync::Arc<AtomicBool>,
) {
    let deadline = Instant::now() + SERVER_LIFETIME;
    let _ = listener.set_nonblocking(true);
    loop {
        if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => handle_conn(stream, token, &cmd_tx),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[GLMeter] 设置页 accept 失败: {e}");
                return;
            }
        }
    }
}

/// 处理单个连接：解析请求行/头/体，路由到表单页或保存
fn handle_conn(mut stream: TcpStream, token: &str, cmd_tx: &Sender<Cmd>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head_end(&head).is_some() || head.len() > MAX_HEAD {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let Some(hdr_end) = head_end(&head) else {
        return;
    };
    let head_str = String::from_utf8_lossy(&head[..hdr_end]);
    let mut lines = head_str.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let mut content_length = 0usize;
    let mut host_ok = false;
    let mut origin: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "host" => {
                    host_ok =
                        v.trim().starts_with("127.0.0.1") || v.trim().starts_with("localhost");
                }
                "origin" => origin = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    // DNS rebinding 防护：Host 必须是本机
    if !host_ok {
        eprintln!("[GLMeter] 设置页 403：Host 头非本机（{path}）");
        respond(&mut stream, 403, "text/plain; charset=utf-8", "forbidden");
        return;
    }
    let token_path = format!("/{token}");
    let path_ok = path == token_path || path == format!("{token_path}/");
    if !path_ok {
        respond(&mut stream, 404, "text/plain; charset=utf-8", "not found");
        return;
    }

    match method {
        "GET" => {
            let cfg = config::load().0;
            let page = render_form(&form_from_config(&cfg), &banners(), None);
            respond(&mut stream, 200, "text/html; charset=utf-8", &page);
        }
        "POST" => {
            // 跨站伪造纵深防御：带明确异源 Origin 的 POST 拒绝。null/缺失放行——
            // 令牌路径 128-bit 随机不可猜、页面 no-referrer 不外泄，token 才是
            // 真正的 CSRF 门闸；且实测 Chrome 在部分环境（扩展改写请求头等）
            // 会对同源表单 POST 发 Origin: null，一刀切会误伤真实用户
            if let Some(o) = &origin {
                let local = o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost");
                if !local && o != "null" {
                    eprintln!("[GLMeter] 设置页 403：POST Origin 异常（{o}）");
                    respond(&mut stream, 403, "text/plain; charset=utf-8", "forbidden");
                    return;
                }
            }
            if content_length > MAX_BODY {
                respond(&mut stream, 413, "text/plain; charset=utf-8", "too large");
                return;
            }
            let mut body = head[hdr_end + 4..].to_vec();
            body.truncate(content_length.min(MAX_BODY));
            while body.len() < content_length && body.len() <= MAX_BODY {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&buf[..n]),
                    Err(_) => return,
                }
            }
            let body = String::from_utf8_lossy(&body);
            match parse_form(&body) {
                Ok(cfg) => match save_config(&cfg) {
                    Ok(()) => {
                        // 保存成功，让托盘立即按新配置刷新
                        let _ = cmd_tx.send(Cmd::Fetch);
                        // 桌面 toast 反馈（跟随桌面通知开关，不发 webhook）：
                        // 浏览器端 JS 无跳转留表单，这里补一个系统级提示
                        if cfg.notify.desktop {
                            notify::send_desktop(
                                "GLMeter · 设置已保存",
                                "配置已写入，托盘下一轮刷新内生效",
                            );
                        }
                        let page = include_str!("../assets/settings_saved.html")
                            .replace("__BACK__", &token_path);
                        respond(&mut stream, 200, "text/html; charset=utf-8", &page);
                    }
                    Err(e) => respond(
                        &mut stream,
                        500,
                        "text/plain; charset=utf-8",
                        &format!("写入配置失败: {e}"),
                    ),
                },
                Err(boxed) => {
                    let (values, msg) = *boxed;
                    let page = render_form(&values, &banners(), Some(&msg));
                    respond(&mut stream, 400, "text/html; charset=utf-8", &page);
                }
            }
        }
        _ => respond(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            "method not allowed",
        ),
    }
}

/// 找到请求头结束位置（\r\n\r\n）
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// 保存配置到 config.toml 并收紧权限
fn save_config(cfg: &Config) -> std::io::Result<()> {
    let path = config::config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let toml = toml::to_string_pretty(cfg).unwrap_or_default();
    std::fs::write(&path, toml)?;
    config::restrict_perms(&path);
    Ok(())
}

/// 生成页面顶部提示横幅（env 覆盖 / 现有文件解析失败）
fn banners() -> String {
    let mut out = String::new();
    let env_key = std::env::var("GLM_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let env_url = std::env::var("GLM_BASE_URL")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if env_key || env_url {
        out.push_str(
            "<div class=\"warn\">⚠ 环境变量 GLM_API_KEY / GLM_BASE_URL 正在覆盖下方对应字段，\
             修改这两个字段需先移除环境变量</div>",
        );
    }
    let path = config::config_path();
    if path.exists() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if toml::from_str::<Config>(&s).is_err() {
                out.push_str(
                    "<div class=\"err\">✗ 当前配置文件解析失败，下方显示的是默认值；\
                     直接保存会<b>覆盖原文件</b>，如需保留请先备份</div>",
                );
            }
        }
    }
    out
}

// ── 表单数据 ────────────────────────────────────────────────────────────────

/// 表单的全部字段（字符串形态，与渲染/解析一一对应）
#[derive(Debug)]
struct FormValues {
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: String,
    interval_secs: String,
    refresh_align: String,
    auto_activate: bool,
    activate_at: String,
    auto_use_reset_card: String,
    tray_title: String,
    desktop: bool,
    remain_pct: String,
    expire_hours: String,
    hook_url: String,
    dingtalk_secret: String,
}

impl FormValues {
    fn blank() -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            max_tokens: String::new(),
            interval_secs: String::new(),
            refresh_align: String::new(),
            auto_activate: false,
            activate_at: String::new(),
            auto_use_reset_card: String::new(),
            tray_title: String::new(),
            desktop: false,
            remain_pct: String::new(),
            expire_hours: String::new(),
            hook_url: String::new(),
            dingtalk_secret: String::new(),
        }
    }
}

fn form_from_config(cfg: &Config) -> FormValues {
    let join = |v: &[String]| v.join(", ");
    FormValues {
        api_key: cfg.api_key.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens.to_string(),
        interval_secs: cfg.interval_secs.to_string(),
        refresh_align: cfg.refresh_align.clone().unwrap_or_default(),
        auto_activate: cfg.auto_activate,
        activate_at: join(&cfg.activate_at),
        auto_use_reset_card: format!("{}", cfg.auto_use_reset_card),
        tray_title: cfg.tray_title.clone(),
        desktop: cfg.notify.desktop,
        remain_pct: join_list_f64(&cfg.notify.remain_pct),
        expire_hours: cfg
            .notify
            .expire_hours
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        hook_url: cfg.notify.hook_url.clone(),
        dingtalk_secret: cfg.notify.dingtalk_secret.clone(),
    }
}

fn join_list_f64(v: &[f64]) -> String {
    v.iter()
        .map(|x| format!("{x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ── 表单解析与校验 ──────────────────────────────────────────────────────────

/// 解析 urlencoded 表单体为 Config；校验失败返回（已填值, 错误信息），
/// 调用方重渲染表单保留用户输入，不落盘。
/// Err 用 Box 装载（FormValues 较大，避免 Result 体积膨胀）
fn parse_form(body: &str) -> Result<Config, Box<(FormValues, String)>> {
    let mut v = FormValues::blank();
    for pair in body.split('&') {
        let Some((k, val)) = pair.split_once('=') else {
            continue;
        };
        let val = percent_decode(val);
        match k {
            "api_key" => v.api_key = val,
            "base_url" => v.base_url = val,
            "model" => v.model = val,
            "max_tokens" => v.max_tokens = val,
            "interval_secs" => v.interval_secs = val,
            "refresh_align" => v.refresh_align = val,
            "auto_activate" => v.auto_activate = true,
            "activate_at" => v.activate_at = val,
            "auto_use_reset_card" => v.auto_use_reset_card = val,
            "tray_title" => v.tray_title = val,
            "desktop" => v.desktop = true,
            "remain_pct" => v.remain_pct = val,
            "expire_hours" => v.expire_hours = val,
            "hook_url" => v.hook_url = val,
            "dingtalk_secret" => v.dingtalk_secret = val,
            _ => {}
        }
    }
    convert_form(&v).map_err(|msg| Box::new((v, msg)))
}

/// 字符串形态 → 强类型 Config，附校验；错误信息直接面向用户（中文）
fn convert_form(v: &FormValues) -> Result<Config, String> {
    let mut cfg = Config {
        api_key: v.api_key.trim().to_string(),
        ..Default::default()
    };
    cfg.base_url = v.base_url.trim().trim_end_matches('/').to_string();
    if cfg.base_url.is_empty() || !cfg.base_url.starts_with("http") {
        return Err("接口地址必须以 http(s):// 开头".into());
    }
    cfg.model = v.model.trim().to_string();
    if cfg.model.is_empty() {
        return Err("模型名不能为空".into());
    }
    cfg.max_tokens = v
        .max_tokens
        .trim()
        .parse()
        .map_err(|_| format!("max_tokens 须为正整数，收到「{}」", v.max_tokens.trim()))?;
    if !(1..=100_000).contains(&cfg.max_tokens) {
        return Err("max_tokens 须在 1 ~ 100000 之间".into());
    }
    cfg.interval_secs = v
        .interval_secs
        .trim()
        .parse()
        .map_err(|_| format!("刷新间隔须为秒数，收到「{}」", v.interval_secs.trim()))?;
    if cfg.interval_secs < 60 {
        return Err("刷新间隔最小 60 秒".into());
    }
    if cfg.interval_secs > 86_400 {
        return Err("刷新间隔最大 86400 秒（一天）".into());
    }
    cfg.refresh_align = match v.refresh_align.trim() {
        "" => None,
        s => Some(normalize_hhmm(s)?),
    };
    cfg.auto_activate = v.auto_activate;
    for s in split_list(&v.activate_at) {
        cfg.activate_at.push(normalize_hhmm(s)?);
    }
    cfg.auto_use_reset_card = v.auto_use_reset_card.trim().parse().map_err(|_| {
        format!(
            "自动用卡阈值须为数字，收到「{}」",
            v.auto_use_reset_card.trim()
        )
    })?;
    if !(0.0..=100.0).contains(&cfg.auto_use_reset_card) {
        return Err("自动用卡阈值须在 0 ~ 100 之间（0 = 关闭）".into());
    }
    cfg.tray_title = v.tray_title.trim().to_string();
    cfg.notify.desktop = v.desktop;
    // Config::default() 自带提醒默认档位，表单语义是「以提交内容为准」，先清空
    cfg.notify.remain_pct.clear();
    cfg.notify.expire_hours.clear();
    for s in split_list(&v.remain_pct) {
        let x: f64 = s
            .parse()
            .map_err(|_| format!("额度提醒阈值须为数字，收到「{s}」"))?;
        if !(0.0..=100.0).contains(&x) {
            return Err(format!("额度提醒阈值 {x} 不在 0 ~ 100 之间"));
        }
        cfg.notify.remain_pct.push(x);
    }
    for s in split_list(&v.expire_hours) {
        let x: u64 = s
            .parse()
            .map_err(|_| format!("到期提醒阈值须为小时数，收到「{s}」"))?;
        cfg.notify.expire_hours.push(x);
    }
    cfg.notify.hook_url = v.hook_url.trim().to_string();
    cfg.notify.dingtalk_secret = v.dingtalk_secret.trim().to_string();
    Ok(cfg)
}

/// "HH:MM" 校验并归一化为两位（"9:5" → "09:05"）
fn normalize_hhmm(s: &str) -> Result<String, String> {
    let s = s.trim();
    let Some((h, m)) = s.split_once(':') else {
        return Err(format!("时刻格式应为 HH:MM，收到「{s}」"));
    };
    let (Ok(h), Ok(m)) = (h.trim().parse::<u32>(), m.trim().parse::<u32>()) else {
        return Err(format!("时刻格式应为 HH:MM，收到「{s}」"));
    };
    if h > 23 || m > 59 {
        return Err(format!("时刻超出范围，收到「{s}」"));
    }
    Ok(format!("{h:02}:{m:02}"))
}

/// 列表输入按中英文逗号/分号/空白切分，去掉空段
fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split([',', '，', ';', '；', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// urlencoded 值解码：'+' → 空格，%XX → 字节
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                match bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(&String::from_utf8_lossy(h), 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// HTML 转义（表单值回显 / 错误信息内嵌）
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &str) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

// ── 页面渲染 ────────────────────────────────────────────────────────────────

/// 表单页：替换模板占位符（不用 format!，CSS 花括号无需转义）
fn render_form(v: &FormValues, banners_html: &str, error: Option<&str>) -> String {
    let chk = |on: bool| if on { "checked" } else { "" };
    let error_banner = match error {
        Some(msg) => format!("<div class=\"err\">✗ {}</div>", html_escape(msg)),
        None => String::new(),
    };
    [
        ("__ERROR__", error_banner),
        ("__BANNERS__", banners_html.to_string()),
        ("__API_KEY__", html_escape(&v.api_key)),
        ("__BASE_URL__", html_escape(&v.base_url)),
        ("__MODEL__", html_escape(&v.model)),
        ("__MAX_TOKENS__", html_escape(&v.max_tokens)),
        ("__INTERVAL__", html_escape(&v.interval_secs)),
        ("__ALIGN__", html_escape(&v.refresh_align)),
        ("__AUTO_ACTIVATE__", chk(v.auto_activate).into()),
        ("__ACTIVATE_AT__", html_escape(&v.activate_at)),
        ("__AUTO_USE__", html_escape(&v.auto_use_reset_card)),
        ("__TRAY_TITLE__", html_escape(&v.tray_title)),
        ("__DESKTOP__", chk(v.desktop).into()),
        ("__REMAIN__", html_escape(&v.remain_pct)),
        ("__EXPIRE__", html_escape(&v.expire_hours)),
        ("__HOOK__", html_escape(&v.hook_url)),
        ("__SECRET__", html_escape(&v.dingtalk_secret)),
    ]
    .into_iter()
    .fold(
        include_str!("../assets/settings_form.html").to_string(),
        |acc, (k, val)| acc.replace(k, &val),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_utf8_and_plus() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("bad%2"), "bad%2");
    }

    #[test]
    fn html_escape_covers_all_meta_chars() {
        assert_eq!(
            html_escape("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn normalize_hhmm_pads_and_rejects() {
        assert_eq!(normalize_hhmm("9:5").unwrap(), "09:05");
        assert_eq!(normalize_hhmm(" 09:00 ").unwrap(), "09:00");
        assert!(normalize_hhmm("24:00").is_err());
        assert!(normalize_hhmm("9:60").is_err());
        assert!(normalize_hhmm("9点").is_err());
    }

    #[test]
    fn split_list_supports_mixed_separators() {
        let v: Vec<&str> = split_list("20, 10；5，1 0.5").collect();
        assert_eq!(v, vec!["20", "10", "5", "1", "0.5"]);
        let v: Vec<&str> = split_list(" , ; ").collect();
        assert!(v.is_empty());
    }

    fn form_body() -> String {
        "api_key=k1.secret&base_url=https%3A%2F%2Fopen.bigmodel.cn&model=glm-5.2&max_tokens=8\
         &interval_secs=60&refresh_align=&activate_at=09%3A00%2C+14%3A30&auto_activate=on\
         &auto_use_reset_card=5&tray_title=GLM+%7B5h_left%7D%25&desktop=on\
         &remain_pct=20%2C10%2C5%2C1&expire_hours=24%2C6%2C1&hook_url=&dingtalk_secret="
            .into()
    }

    #[test]
    fn parse_form_roundtrip_full_valid_body() {
        let cfg = parse_form(&form_body()).unwrap();
        assert_eq!(cfg.api_key, "k1.secret");
        assert_eq!(cfg.base_url, "https://open.bigmodel.cn");
        assert_eq!(cfg.model, "glm-5.2");
        assert_eq!(cfg.max_tokens, 8);
        assert_eq!(cfg.interval_secs, 60);
        assert_eq!(cfg.refresh_align, None);
        assert_eq!(cfg.activate_at, vec!["09:00", "14:30"]);
        assert!(cfg.auto_activate);
        assert_eq!(cfg.auto_use_reset_card, 5.0);
        assert_eq!(cfg.tray_title, "GLM {5h_left}%");
        assert!(cfg.notify.desktop);
        assert_eq!(cfg.notify.remain_pct, vec![20.0, 10.0, 5.0, 1.0]);
        assert_eq!(cfg.notify.expire_hours, vec![24, 6, 1]);
        assert_eq!(cfg.notify.hook_url, "");
    }

    #[test]
    fn parse_form_without_checkboxes_means_off() {
        let body = "api_key=&base_url=https://x&model=m&max_tokens=8&interval_secs=300\
                    &auto_use_reset_card=0&tray_title=&remain_pct=&expire_hours=";
        let cfg = parse_form(body).unwrap();
        assert!(!cfg.auto_activate);
        assert!(!cfg.notify.desktop);
        assert!(cfg.notify.remain_pct.is_empty(), "空列表 = 关闭额度提醒");
        assert!(cfg.notify.expire_hours.is_empty());
        assert!(cfg.activate_at.is_empty());
        assert_eq!(cfg.refresh_align, None);
    }

    #[test]
    fn parse_form_rejects_bad_values_with_message() {
        let body = form_body().replace("interval_secs=60", "interval_secs=5");
        let (_, msg) = *parse_form(&body).unwrap_err();
        assert!(msg.contains("60"), "{msg}");
        let body = form_body().replace("max_tokens=8", "max_tokens=x");
        let (_, msg) = *parse_form(&body).unwrap_err();
        assert!(msg.contains("max_tokens"), "{msg}");
        let body = form_body().replace("refresh_align=", "refresh_align=25%3A00");
        let (_, msg) = *parse_form(&body).unwrap_err();
        assert!(msg.contains("25:00"), "{msg}");
        let body = form_body().replace("remain_pct=20", "remain_pct=120");
        let (_, msg) = *parse_form(&body).unwrap_err();
        assert!(msg.contains("0 ~ 100"), "{msg}");
    }

    #[test]
    fn form_from_config_then_parse_is_identity() {
        let cfg = parse_form(&form_body()).unwrap();
        let values = form_from_config(&cfg);
        let cfg2 = convert_form(&values).unwrap();
        assert_eq!(cfg.api_key, cfg2.api_key);
        assert_eq!(cfg.activate_at, cfg2.activate_at);
        assert_eq!(cfg.notify.remain_pct, cfg2.notify.remain_pct);
        assert_eq!(cfg.notify.expire_hours, cfg2.notify.expire_hours);
        assert_eq!(cfg.refresh_align, cfg2.refresh_align);
        assert_eq!(cfg.tray_title, cfg2.tray_title);
    }

    // ── 端到端：真实起服务，HTTP 层全链路（GLMETER_CONFIG 指向临时文件，不碰真实配置）──
    // Read/Write/TcpStream 已由 use super::* 从模块级继承

    /// 发送原始请求并读取完整响应
    fn http(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn post_req(port: u16, token: &str, body: &str) -> String {
        format!(
            "POST /{token}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn server_e2e_form_fetch_save_and_authz() {
        let tmp =
            std::env::temp_dir().join(format!("glmeter-settings-e2e-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::env::set_var("GLMETER_CONFIG", &tmp);

        let (tx, rx) = std::sync::mpsc::channel();
        let url = ensure_server(tx).expect("服务应正常起监听");
        let (port, token) = {
            let active = ACTIVE.lock().unwrap();
            let s = active.as_ref().unwrap();
            (s.port, s.token.clone())
        };
        assert!(url.contains(&port.to_string()) && url.contains(&token));

        // 1) GET 表单页：200 + 页面关键内容
        let page = http(
            port,
            &format!("GET /{token}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{}", &page[..40]);
        assert!(page.contains("GLMeter 设置"));
        assert!(page.contains("保存配置"));

        // 2) POST 合法表单：200 成功页 + 落盘 + 0600 + worker 收到 Fetch
        let body = "api_key=k&base_url=https%3A%2F%2Fopen.bigmodel.cn&model=m&max_tokens=8\
                    &interval_secs=120&auto_use_reset_card=0&tray_title=E2E&remain_pct=&expire_hours=";
        let resp = http(port, &post_req(port, &token, body));
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", &resp[..40]);
        assert!(resp.contains("已保存"));
        let saved = std::fs::read_to_string(&tmp).unwrap();
        assert!(saved.contains("tray_title = \"E2E\""), "{saved}");
        assert!(saved.contains("interval_secs = 120"), "{saved}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&tmp).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "写盘后须收紧为 owner-only");
        }
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Cmd::Fetch)),
            "保存后应向 worker 发送 Fetch"
        );

        // 3) POST 非法值：400 + 错误信息 + 文件未被改动
        let bad = body.replace("interval_secs=120", "interval_secs=5");
        let resp = http(port, &post_req(port, &token, &bad));
        assert!(resp.starts_with("HTTP/1.1 400"), "{}", &resp[..40]);
        assert!(resp.contains("60"));
        assert!(std::fs::read_to_string(&tmp)
            .unwrap()
            .contains("interval_secs = 120"));

        // 4) 鉴权：错误令牌 404；伪造 Host（DNS rebinding）403
        let resp = http(
            port,
            &format!("GET /wrong-token/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 404"));
        let resp = http(
            port,
            &format!("GET /{token}/ HTTP/1.1\r\nHost: evil.example.com\r\n\r\n"),
        );
        assert!(resp.starts_with("HTTP/1.1 403"));

        // 5) Origin 策略：明确异源 403；null/缺失放行（Chrome 部分环境会对
        //    同源表单 POST 发 null，token 才是真正的 CSRF 门闸）
        let mut foreign = post_req(port, &token, body);
        foreign = foreign.replacen("\r\n\r\n", "\r\nOrigin: http://evil.example.com\r\n\r\n", 1);
        let resp = http(port, &foreign);
        assert!(resp.starts_with("HTTP/1.1 403"), "{}", &resp[..40]);
        let mut nulled = post_req(port, &token, body);
        nulled = nulled.replacen("\r\n\r\n", "\r\nOrigin: null\r\n\r\n", 1);
        let resp = http(port, &nulled);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", &resp[..40]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn render_form_contains_escaped_values_and_checked_flags() {
        let mut v = FormValues::blank();
        v.tray_title = "<b>&".into();
        v.auto_activate = true;
        let page = render_form(&v, "", Some("出<错>"));
        assert!(page.contains("value=\"&lt;b&gt;&amp;\""), "值须转义回填");
        assert!(page.contains("checked"));
        assert!(page.contains("出&lt;错&gt;"), "错误信息须转义");
        // 横幅位：无错误时也应填入（可为空），不留占位符残留
        assert!(!page.contains("__BANNERS__"));
        assert!(!page.contains("__ERROR__"));
    }
}
