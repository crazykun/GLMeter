//! 自动用卡：存在 2 小时内过期的重置卡、且 5 小时额度剩余低于阈值
//! （`auto_use_reset_card`，0 = 关闭）时，自动使用最早过期的一张（不限类型）。
//!
//! 触发即自愈：用卡成功后额度重置回满，条件自然解除，天然防连发；
//! 失败则指数退避，不换 requestId 立即重试——下一轮刷新会重新拉取卡列表，
//! 若卡实际已被消耗（超时等未知结果），额度已重置、条件同样解除。

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, Utc};

use crate::api::{self, QuotaSnapshot, ResetRecord};
use crate::config::Config;
use crate::{notify, set_busy, worker_fetch, UiState};

/// 卡片「快过期」判定窗口：剩余有效期 ≤ 2 小时
const EXPIRE_WINDOW_MINS: i64 = 120;
/// 用卡失败退避：5 分钟起步、每失败一轮翻倍、封顶 1 小时
const BACKOFF_BASE_SECS: u64 = 300;
const BACKOFF_MAX_SECS: u64 = 3600;

/// 下次允许自动用卡的 epoch 秒（失败退避闸门，0 = 无限制）与连续失败轮数
static NEXT_TRY_SECS: AtomicI64 = AtomicI64::new(0);
static FAILED_ROUNDS: AtomicU32 = AtomicU32::new(0);

/// 一次自动用卡决策
pub struct Decision {
    /// true = 周卡（同步重置 5h 额度），false = 5h 卡
    pub week: bool,
    pub record: ResetRecord,
    /// 触发时 5 小时窗口的剩余百分比
    pub remaining: f64,
    /// 卡片剩余有效期（分钟）
    pub mins_left: i64,
}

/// 纯决策：开启且 5h 额度剩余 < threshold、存在 2h 内过期的可用卡时，
/// 返回最早过期的一张；否则 None。now 作参数便于测试。
pub fn decide(snap: &QuotaSnapshot, threshold: f64, now: DateTime<Local>) -> Option<Decision> {
    if !threshold.is_finite() || threshold <= 0.0 {
        return None;
    }
    let window = snap.five_hour()?;
    let remaining = 100.0 - window.used_pct;
    if remaining >= threshold {
        return None;
    }
    let resets = snap.resets.as_ref()?;
    // 两类卡各取最早过期的可用卡，再整体取更早的一张
    let candidates = [
        resets.pick(false).map(|r| (false, r)),
        resets.pick(true).map(|r| (true, r)),
    ];
    let mut best: Option<(bool, &ResetRecord, i64)> = None;
    for (week, rec) in candidates.into_iter().flatten() {
        let Some(expire) = notify::parse_expire(&rec.expire_time) else {
            continue;
        };
        let mins_left = (expire - now).num_minutes();
        if mins_left <= 0 || mins_left > EXPIRE_WINDOW_MINS {
            continue;
        }
        if best.as_ref().is_none_or(|(_, _, m)| mins_left < *m) {
            best = Some((week, rec, mins_left));
        }
    }
    let (week, record, mins_left) = best?;
    Some(Decision {
        week,
        record: record.clone(),
        remaining,
        mins_left,
    })
}

/// worker_fetch 每轮末尾调用（Status::Ok 时 snapshot 非空）：命中决策则静默用卡
pub fn maybe_auto_use(
    client: &reqwest::blocking::Client,
    state: &Arc<Mutex<UiState>>,
    snap: &QuotaSnapshot,
    cfg: &Config,
) {
    let Some(d) = decide(snap, cfg.auto_use_reset_card, Local::now()) else {
        // 条件不满足（含卡已被用掉/额度已重置）：清零失败计数，下次失败从最短退避重来
        FAILED_ROUNDS.store(0, Ordering::Relaxed);
        return;
    };
    let next_try = NEXT_TRY_SECS.load(Ordering::Relaxed);
    if next_try > 0 && Utc::now().timestamp() < next_try {
        return;
    }
    use_card(client, state, snap, cfg, d);
}

fn use_card(
    client: &reqwest::blocking::Client,
    state: &Arc<Mutex<UiState>>,
    snap: &QuotaSnapshot,
    cfg: &Config,
    d: Decision,
) {
    let kind = if d.week { "周额度" } else { "5小时额度" };
    set_busy(state, Some(format!("自动使用{kind}重置卡…")));
    let request_id = api::new_request_id();
    match api::use_reset_card(client, cfg, d.week, d.record.record_id, &request_id) {
        Ok(()) => {
            FAILED_ROUNDS.store(0, Ordering::Relaxed);
            NEXT_TRY_SECS.store(0, Ordering::Relaxed);
            eprintln!(
                "[GLMeter] 自动用卡成功（{kind}，recordId={}，触发时 5h 剩余 {:.1}%）",
                d.record.record_id, d.remaining
            );
            // 登记自己刚用掉的卡：随后的刷新里不计入「已使用」提醒
            notify::mark_self_used(d.record.record_id);
            let left = snap
                .resets
                .as_ref()
                .map(|r| r.available_count(d.week).saturating_sub(1))
                .unwrap_or(0);
            notify::push(
                "GLMeter · 自动用卡",
                &format!(
                    "5小时额度剩余 {:.1}%，一张「{kind}」重置卡 {} 分钟后过期，\
                     已自动使用并重置额度（剩 {left} 张可用）",
                    d.remaining, d.mins_left
                ),
                cfg,
            );
            // 立即刷新，让托盘与通知状态反映用卡后的新额度
            worker_fetch(client, state);
        }
        Err(e) => {
            let rounds = FAILED_ROUNDS.fetch_add(1, Ordering::Relaxed) + 1;
            let secs = (BACKOFF_BASE_SECS << (rounds - 1).min(12)).min(BACKOFF_MAX_SECS);
            NEXT_TRY_SECS.store(Utc::now().timestamp() + secs as i64, Ordering::Relaxed);
            eprintln!(
                "[GLMeter] 自动用卡失败（{kind}，recordId={}，requestId={}）：{e}，{secs} 秒后再试",
                d.record.record_id, request_id
            );
            set_busy(state, Some(format!("自动用卡失败: {e}")));
            notify::push(
                "GLMeter · 自动用卡失败",
                &format!(
                    "一张「{kind}」重置卡 {} 分钟后过期、5小时额度仅剩 {:.1}%，\
                     自动用卡失败：{e}\n可在托盘菜单手动使用",
                    d.mins_left, d.remaining
                ),
                cfg,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ResetCards, TokenWindow};
    use chrono::Timelike;

    /// 测试用「整秒 now」：expire 格式化只保留到秒（%H:%M:%S），
    /// 若 now 带亚秒，格式化往返一次丢掉亚秒，num_minutes 截断后会差 1 分钟
    fn whole_second_now() -> DateTime<Local> {
        Local::now().with_nanosecond(0).unwrap()
    }

    fn snap_with(used_pct: f64, resets: Option<ResetCards>) -> QuotaSnapshot {
        QuotaSnapshot {
            level: "lite".into(),
            windows: vec![TokenWindow {
                label: "5小时额度".into(),
                used_pct,
                activated: true,
                next_reset: None,
            }],
            mcp: None,
            fetched_at: Local::now(),
            resets,
        }
    }

    fn card(record_id: i64, mins_from_now: i64, now: DateTime<Local>) -> ResetRecord {
        ResetRecord {
            record_id,
            expire_time: (now + chrono::Duration::minutes(mins_from_now))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            available: true,
        }
    }

    fn cards(five_hour: Vec<ResetRecord>, week: Vec<ResetRecord>) -> ResetCards {
        ResetCards {
            five_hour,
            week,
            last_five_hour_reset: None,
            last_week_reset: None,
        }
    }

    #[test]
    fn off_when_threshold_not_positive() {
        let now = whole_second_now();
        let s = snap_with(99.0, Some(cards(vec![card(1, 30, now)], vec![])));
        assert!(decide(&s, 0.0, now).is_none(), "0 = 关闭");
        assert!(decide(&s, -1.0, now).is_none());
    }

    #[test]
    fn no_trigger_when_quota_remaining_high() {
        let now = whole_second_now();
        let s = snap_with(30.0, Some(cards(vec![card(1, 30, now)], vec![])));
        assert!(decide(&s, 5.0, now).is_none(), "剩余 70% ≥ 5% 不触发");
    }

    #[test]
    fn no_trigger_without_cards_or_resets() {
        let now = whole_second_now();
        let s = snap_with(99.0, None);
        assert!(decide(&s, 5.0, now).is_none(), "resets 拉取失败不触发");
        let s = snap_with(99.0, Some(cards(vec![], vec![])));
        assert!(decide(&s, 5.0, now).is_none(), "没有可用卡不触发");
    }

    #[test]
    fn no_trigger_when_card_expires_beyond_window() {
        let now = whole_second_now();
        let s = snap_with(99.0, Some(cards(vec![card(1, 181, now)], vec![])));
        assert!(decide(&s, 5.0, now).is_none(), "3 小时后才过期，不急于用");
    }

    #[test]
    fn already_expired_card_excluded() {
        let now = whole_second_now();
        let s = snap_with(99.0, Some(cards(vec![card(1, -10, now)], vec![])));
        assert!(decide(&s, 5.0, now).is_none(), "已过期（不可用）不触发");
    }

    #[test]
    fn boundary_two_hours_included_excluded_above() {
        let now = whole_second_now();
        let s = snap_with(99.0, Some(cards(vec![card(1, 120, now)], vec![])));
        assert!(decide(&s, 5.0, now).is_some(), "恰好 2 小时算「快过期」");
        let s = snap_with(99.0, Some(cards(vec![card(1, 121, now)], vec![])));
        assert!(decide(&s, 5.0, now).is_none());
    }

    #[test]
    fn picks_earliest_across_types() {
        let now = whole_second_now();
        // 5h 卡 90 分钟后过期，周卡 30 分钟后过期 → 应选周卡
        let s = snap_with(
            99.0,
            Some(cards(vec![card(11, 90, now)], vec![card(22, 30, now)])),
        );
        let d = decide(&s, 5.0, now).unwrap();
        assert!(d.week);
        assert_eq!(d.record.record_id, 22);
        assert_eq!(d.mins_left, 30);
        // 反过来：5h 卡更早 → 选 5h 卡
        let s = snap_with(
            99.0,
            Some(cards(vec![card(11, 20, now)], vec![card(22, 50, now)])),
        );
        let d = decide(&s, 5.0, now).unwrap();
        assert!(!d.week);
        assert_eq!(d.record.record_id, 11);
    }

    #[test]
    fn unavailable_card_skipped() {
        let now = whole_second_now();
        let mut c = card(1, 30, now);
        c.available = false;
        let s = snap_with(99.0, Some(cards(vec![c], vec![])));
        assert!(decide(&s, 5.0, now).is_none(), "pick 只在可用卡里选");
    }

    #[test]
    fn unparseable_expire_time_skipped() {
        let now = whole_second_now();
        let bad = ResetRecord {
            record_id: 1,
            expire_time: "not-a-date".into(),
            available: true,
        };
        let s = snap_with(99.0, Some(cards(vec![bad, card(2, 30, now)], vec![])));
        let d = decide(&s, 5.0, now).unwrap();
        assert_eq!(d.record.record_id, 2, "坏时间戳的卡跳过，选正常的");
    }
}
