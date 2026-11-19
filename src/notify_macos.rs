//! macOS 原生通知：GLMeter.app 自己持有 UNUserNotificationCenter，
//! 避免 osascript 通知被系统归属到「脚本编辑器」、点击后打开空白文稿。

use crate::Cmd;
use block2::{DynBlock, RcBlock};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

const BUNDLE_ID: &str = "com.crazykun.glmeter";
static COMMAND: OnceLock<Sender<Cmd>> = OnceLock::new();

define_class!(
    // SAFETY: NSObject 无额外子类要求；委托没有 ivars，也不实现 Drop。
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        // 托盘进程一直存活，应用处于前台时也要正常展示横幅。
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            // 只响应点击正文；划掉/关闭通知不能打开设置页。
            let default_action = unsafe { UNNotificationDefaultActionIdentifier };
            if response.actionIdentifier().isEqualToString(default_action) {
                if let Some(cmd) = COMMAND.get() {
                    let _ = cmd.send(Cmd::Settings);
                }
            }
            completion_handler.call(());
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        // SAFETY: 与 NSObject 的 init 签名一致。
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// 在事件循环启动前注册委托；center 的 delegate 是弱引用，委托须存活到进程退出。
pub(crate) fn init(cmd_tx: Sender<Cmd>) {
    if !has_app_bundle() {
        return;
    }
    autoreleasepool(|_| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = NotificationDelegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        std::mem::forget(delegate);
        let _ = COMMAND.set(cmd_tx);
    });
}

pub(super) fn send(title: &str, body: &str) {
    // UserNotifications 要求 app bundle；Homebrew Formula / cargo run 等
    // 裸二进制场景保留旧实现，避免 currentNotificationCenter 直接抛异常。
    if !has_app_bundle() {
        send_legacy(title, body);
        return;
    }
    autoreleasepool(|_| send_native(title, body));
}

fn has_app_bundle() -> bool {
    NSBundle::mainBundle()
        .bundleIdentifier()
        .is_some_and(|id| id.to_string() == BUNDLE_ID)
}

fn send_native(title: &str, body: &str) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));

    let identifier = NSString::from_str(&crate::api::new_request_id());
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);
    let center_for_auth = center.clone();
    let on_authorized = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        if let Some(error) = error_text(error) {
            eprintln!("[GLMeter] 请求通知权限失败: {error}");
            return;
        }
        if !granted.as_bool() {
            eprintln!("[GLMeter] 桌面通知未发送：GLMeter 通知权限未开启");
            return;
        }
        let on_added = RcBlock::new(|error: *mut NSError| {
            if let Some(error) = error_text(error) {
                eprintln!("[GLMeter] 桌面通知发送失败: {error}");
            }
        });
        center_for_auth.addNotificationRequest_withCompletionHandler(&request, Some(&on_added));
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert,
        &on_authorized,
    );
}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: UserNotifications 的 completion handler 保证非空指针指向有效 NSError。
    (!error.is_null()).then(|| unsafe { (&*error).localizedDescription().to_string() })
}

/// 非 app bundle 的兼容路径。osascript 无法设置通知归属与点击动作。
fn send_legacy(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let ok = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("[GLMeter] 桌面通知失败（osascript 不可用）");
    }
}
