//! 平台无关的剪贴板观察契约。
//!
//! Windows/macOS 后端把一次抓取整理成 [`ClipboardObservation`]，再由
//! [`classify_event`] 与上一份快照比较，决定这次变化该不该落库、以什么
//! 语义落库。这里有平台 cfg，Linux 也能编译并单测整条判定链路。
//!
//! 与 X11 后端的分工：X11 通过 XFixes/RECORD 直接拿到"谁写/谁读"的协议事件，
//! 读取是可靠的；Windows/macOS 的 OS 不提供全局读取拦截。Windows 的推断读取
//! 由 `read_signal.rs` + `App::record_read_event` 独立落库，永远带
//! `op="read"` + `reason="inferred_read"` 并在摘要写明判定依据，
//! 绝不冒充可靠审计；macOS 不产生读取事件。

use crate::classify::Captured;

/// OS 可以可靠给出的"内容生产者"信息。`wm_class` 是 X11 概念，其他平台恒空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceInfo {
    pub pid: Option<i32>,
    pub app_name: String,
    pub exec_path: String,
    /// 窗口 / 所有者句柄，统一收敛为 u32 以匹配 `NewEvent.window_id`。
    pub window_id: u32,
}

impl SourceInfo {
    pub fn has_any(&self) -> bool {
        self.pid.is_some()
            || !self.app_name.is_empty()
            || !self.exec_path.is_empty()
            || self.window_id != 0
    }
}

/// "触发主体"：动作发生时前台的应用/窗口。与内容生产者可能不同（例如
/// 从 A 复制、在 B 粘贴），单列以免把两者混为一谈。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriggerInfo {
    pub pid: Option<i32>,
    pub app_name: String,
    pub window_title: String,
}

impl TriggerInfo {
    pub fn has_any(&self) -> bool {
        self.pid.is_some() || !self.app_name.is_empty() || !self.window_title.is_empty()
    }
}

/// 一次疑似读取（粘贴）的间接信号，供 Windows 推断读取使用。
/// 平台无关定义，便于在 Linux 单测里覆盖落库逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSignal {
    /// 触发来源：`open_clipboard` / `ctrl_v` / `both`。
    pub evidence: &'static str,
    /// 持有剪贴板的窗口对应进程（读取方），可能取不到。
    pub reader: Option<SourceInfo>,
    /// Ctrl+V 发生时的前台应用（触发主体），可能取不到。
    pub trigger: Option<TriggerInfo>,
}

/// 一次剪贴板抓取的完整观察结果。
#[derive(Debug, Clone)]
pub struct ClipboardObservation {
    /// 本次可用格式清单（标准名，如 `text/plain`、`image/x-rgba`、
    /// `CF_HDROP`）。已排序去重，便于比较。
    pub formats: Vec<String>,
    /// OS 变更序号：Windows `GetClipboardSequenceNumber`、macOS
    /// `NSPasteboard.changeCount`；Linux 的 portable 路径不用，为 None。
    pub sequence: Option<u64>,
    /// 共享 `classify::assemble` 的产出（含 sha256）。
    pub body: Captured,
    pub source: Option<SourceInfo>,
    /// 来源归属方式（`clipboard_owner` / `foreground_fallback` /
    /// `frontmost_app`），写进 meta_json 以免把回退值当成所有者。
    pub source_attribution: Option<String>,
    pub trigger: Option<TriggerInfo>,
    pub is_empty: bool,
    /// 该次通知是否疑似"读取触发的延迟渲染"。
    ///
    /// 目前 Windows 的推断读取走 `App::record_read_event` 独立落库（由
    /// `read_signal.rs` 的 OpenClipboard 轮询 + Ctrl+V 钩子驱动），不经过
    /// 内容抓取管线，因此生产代码里此字段恒为 false。保留该字段与
    /// `classify_event` 的对应分支，供未来 OS 提供读取信号时复用；单测
    /// 覆盖其判定契约。
    pub inferred_read: bool,
}

impl ClipboardObservation {
    pub fn new(body: Captured) -> Self {
        Self {
            formats: Vec::new(),
            sequence: None,
            body,
            source: None,
            source_attribution: None,
            trigger: None,
            is_empty: false,
            inferred_read: false,
        }
    }

    /// 去重键：内容摘要 + 格式清单 + 变更序号。
    ///
    /// 单看 sha256 会漏掉"内容相同但格式集合变了"（例如复制同一张图时
    /// 附带/去掉 HTML）；单看序号会把"序号变但内容没变"误报成新事件。
    /// 三者合起来才既能识别真实变化、又能识别纯格式/纯序号变化。
    pub fn fingerprint(&self) -> String {
        let formats = self.formats.join(",");
        match self.sequence {
            Some(seq) => format!("{}|{}|{}", self.body.sha256, formats, seq),
            None => format!("{}|{}|-", self.body.sha256, formats),
        }
    }
}

/// 变化语义。`reason` 与 `meta_json.change` 用同一组词，方便前端展示与筛选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 常规内容变化（首次快照也归此类，带 `first` 标记）。
    Content,
    /// 剪贴板被清空。
    Cleared,
    /// 内容摘要相同、格式集合变了。
    FormatOnly,
    /// 内容与格式都相同、只有 OS 序号变了（例如重新声明所有权）。
    SequenceOnly,
    /// 后端启动/重新启用后的首次快照。
    Snapshot,
    /// 暂停后重新启用时抓到的内容，与暂停前相同也应当留痕。
    Reenabled,
    /// 疑似读取触发的延迟渲染（推断，非可靠审计）。
    InferredRead,
}

impl ChangeKind {
    pub fn as_reason(self) -> &'static str {
        match self {
            ChangeKind::Content => "content_change",
            ChangeKind::Cleared => "cleared",
            ChangeKind::FormatOnly => "format_change",
            ChangeKind::SequenceOnly => "sequence_change",
            ChangeKind::Snapshot => "snapshot",
            ChangeKind::Reenabled => "reenabled",
            ChangeKind::InferredRead => "inferred_read",
        }
    }

    /// 是否属于"写入"语义（其余为推断读取）。
    pub fn is_write(self) -> bool {
        !matches!(self, ChangeKind::InferredRead)
    }
}

/// 落库决定。[`classify_event`] 的纯输出，不含任何 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSpec {
    pub op: &'static str,
    pub reason: &'static str,
    pub kind: ChangeKind,
    /// 是否为推断事件（写入 UI/文档/元数据标注）。
    pub inferred: bool,
    /// 是否为后端启动后的第一条记录。
    pub first: bool,
}

impl EventSpec {
    /// `meta_json.change` 子对象，供详情页说明这次事件的判定依据。
    pub fn change_meta(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.reason,
            "op": self.op,
            "inferred": self.inferred,
            "first": self.first,
        })
    }
}

/// 上一份已落库观察的快照（判定去重与变化语义所需的最小集合）。
#[derive(Debug, Clone, Default)]
pub struct ObservationSnapshot {
    pub fingerprint: String,
    pub content_sha256: String,
    pub formats: Vec<String>,
    pub sequence: Option<u64>,
    pub is_empty: bool,
    /// 暂停过且尚未重新启用后成功记录过内容。
    pub suspended: bool,
}

impl ObservationSnapshot {
    pub fn from_observation(obs: &ClipboardObservation) -> Self {
        Self {
            fingerprint: obs.fingerprint(),
            content_sha256: obs.body.sha256.clone(),
            formats: obs.formats.clone(),
            sequence: obs.sequence,
            is_empty: obs.is_empty,
            suspended: false,
        }
    }
}

/// 纯函数：把一次观察与上一份快照比较，得出该不该落库、以什么语义落库。
///
/// 返回 `None` 表示"没有可记录的变化"（内容/格式/序号全同且未暂停过），
/// 调用方应清掉上一次的瞬时错误但不写库。这样 A→B→A 这类变化不会因为
/// 最终回到 A 就被整轮吞掉：只要中间态被采到，指纹变化就会被记录。
pub fn classify_event(
    last: Option<&ObservationSnapshot>,
    obs: &ClipboardObservation,
) -> Option<EventSpec> {
    let fingerprint = obs.fingerprint();
    let Some(prev) = last else {
        return Some(EventSpec {
            op: "write",
            reason: ChangeKind::Snapshot.as_reason(),
            kind: ChangeKind::Snapshot,
            inferred: false,
            first: true,
        });
    };

    // 暂停/恢复：即使内容没变也留一条 `reenabled`，并清掉 suspended 标记，
    // 避免暂停期间的变化只留下最终态一条。
    if prev.suspended {
        return Some(EventSpec {
            op: "write",
            reason: ChangeKind::Reenabled.as_reason(),
            kind: ChangeKind::Reenabled,
            inferred: false,
            first: false,
        });
    }

    // 推断读取优先判定：序号没变但收到变更通知，且 OS 给出延迟渲染信号。
    // 这类事件单独成行（op=read），不与写入混在一起。
    if obs.inferred_read && obs.sequence.is_some() && prev.sequence == obs.sequence && !obs.is_empty
    {
        return Some(EventSpec {
            op: "read",
            reason: ChangeKind::InferredRead.as_reason(),
            kind: ChangeKind::InferredRead,
            inferred: true,
            first: false,
        });
    }

    if prev.fingerprint == fingerprint {
        return None;
    }

    let kind = if obs.is_empty && !prev.is_empty {
        ChangeKind::Cleared
    } else if prev.content_sha256 == obs.body.sha256 && prev.formats != obs.formats {
        ChangeKind::FormatOnly
    } else if prev.content_sha256 == obs.body.sha256
        && prev.formats == obs.formats
        && prev.sequence != obs.sequence
    {
        ChangeKind::SequenceOnly
    } else {
        ChangeKind::Content
    };

    Some(EventSpec {
        op: "write",
        reason: kind.as_reason(),
        kind,
        inferred: false,
        first: false,
    })
}

/// 白名单判断的参数打包，便于在 Linux 单测里覆盖跨平台分支。
#[derive(Debug, Clone, Default)]
pub struct WhitelistContext<'a> {
    pub list: &'a [String],
    pub self_pid: u32,
    pub self_exe: &'a str,
}

/// 纯函数：来源信息是否命中白名单。复用 `config::is_whitelisted`，
/// `wm_class` 传空串（非 X11 平台无此概念）。命中时正文零落盘。
pub fn is_source_whitelisted(source: Option<&SourceInfo>, ctx: &WhitelistContext<'_>) -> bool {
    let Some(info) = source else {
        return false;
    };
    crate::config::is_whitelisted(
        &info.app_name,
        "",
        &info.exec_path,
        ctx.list,
        ctx.self_pid,
        info.pid,
        ctx.self_exe,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{empty_capture, Captured};

    fn observation(sha: &str, formats: &[&str], sequence: Option<u64>) -> ClipboardObservation {
        let mut cap = Captured::placeholder("test");
        cap.sha256 = sha.to_string();
        let mut obs = ClipboardObservation::new(cap);
        obs.formats = formats.iter().map(|s| (*s).to_string()).collect();
        obs.sequence = sequence;
        obs
    }

    fn empty_observation() -> ClipboardObservation {
        let mut obs = ClipboardObservation::new(empty_capture());
        obs.is_empty = true;
        obs
    }

    #[test]
    fn first_observation_is_snapshot_even_for_empty_clipboard() {
        let spec = classify_event(None, &empty_observation()).expect("首条快照应落库");
        assert_eq!(spec.op, "write");
        assert_eq!(spec.reason, "snapshot");
        assert!(spec.first);
        assert_eq!(spec.change_meta()["inferred"], false);
    }

    #[test]
    fn identical_content_formats_and_sequence_is_deduplicated() {
        let obs = observation("sha-A", &["text/plain"], Some(7));
        let prev = ObservationSnapshot::from_observation(&obs);
        assert_eq!(classify_event(Some(&prev), &obs), None);
    }

    #[test]
    fn content_change_is_recorded_and_a_b_a_return_is_not_swallowed() {
        let a = observation("sha-A", &["text/plain"], Some(1));
        let b = observation("sha-B", &["text/plain"], Some(2));
        let mut prev = ObservationSnapshot::from_observation(&a);
        let spec = classify_event(Some(&prev), &b).expect("A→B 应记录");
        assert_eq!(spec.reason, "content_change");
        prev = ObservationSnapshot::from_observation(&b);
        let back = observation("sha-A", &["text/plain"], Some(3));
        let spec = classify_event(Some(&prev), &back).expect("B→A 同样应记录");
        assert_eq!(spec.reason, "content_change");
    }

    #[test]
    fn clearing_clipboard_gets_cleared_reason() {
        let a = observation("sha-A", &["text/plain"], Some(1));
        let prev = ObservationSnapshot::from_observation(&a);
        let spec = classify_event(Some(&prev), &empty_observation()).expect("清空应记录");
        assert_eq!(spec.op, "write");
        assert_eq!(spec.reason, "cleared");
    }

    #[test]
    fn format_only_and_sequence_only_changes_are_distinguished() {
        let base = observation("sha-A", &["text/plain"], Some(1));
        let prev = ObservationSnapshot::from_observation(&base);

        let more_formats = observation("sha-A", &["text/plain", "text/html"], Some(1));
        let spec = classify_event(Some(&prev), &more_formats).expect("格式变化应记录");
        assert_eq!(spec.reason, "format_change");

        let new_seq = observation("sha-A", &["text/plain"], Some(2));
        let spec = classify_event(Some(&prev), &new_seq).expect("序号变化应记录");
        assert_eq!(spec.reason, "sequence_change");
    }

    #[test]
    fn resuming_after_suspension_records_even_unchanged_content() {
        let obs = observation("sha-A", &["text/plain"], Some(1));
        let mut prev = ObservationSnapshot::from_observation(&obs);
        prev.suspended = true;
        let spec = classify_event(Some(&prev), &obs).expect("恢复后应留痕");
        assert_eq!(spec.reason, "reenabled");
        assert!(!spec.inferred);
    }

    #[test]
    fn inferred_read_is_explicitly_marked_and_only_when_sequence_is_stable() {
        let base = observation("sha-A", &["text/plain"], Some(9));
        let prev = ObservationSnapshot::from_observation(&base);

        let mut same_seq = observation("sha-A", &["text/plain"], Some(9));
        same_seq.inferred_read = true;
        let spec = classify_event(Some(&prev), &same_seq).expect("推断读取应记录");
        assert_eq!(spec.op, "read");
        assert_eq!(spec.reason, "inferred_read");
        assert!(spec.inferred);
        assert_eq!(spec.change_meta()["inferred"], true);

        // 序号也变了 → 视为真实写入，不误判成读取。
        let mut changed_seq = observation("sha-A", &["text/plain"], Some(10));
        changed_seq.inferred_read = true;
        let spec = classify_event(Some(&prev), &changed_seq).expect("序号变化应是写入");
        assert_eq!(spec.op, "write");
        assert!(!spec.inferred);

        // 空剪贴板不推断读取。
        let mut empty = empty_observation();
        empty.sequence = Some(9);
        empty.inferred_read = true;
        let spec = classify_event(Some(&prev), &empty).expect("清空仍按写入处理");
        assert_eq!(spec.op, "write");
        assert_eq!(spec.reason, "cleared");
    }

    #[test]
    fn fingerprint_includes_formats_and_sequence() {
        let a = observation("sha-A", &["text/plain"], Some(1));
        let b = observation("sha-A", &["text/plain", "text/html"], Some(1));
        let c = observation("sha-A", &["text/plain"], Some(2));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(
            a.fingerprint(),
            observation("sha-A", &["text/plain"], Some(1)).fingerprint()
        );
    }

    #[test]
    fn whitelist_matches_pid_name_and_path_and_never_matches_missing_source() {
        let ctx = WhitelistContext {
            list: &["1234".into(), "SecretApp".into(), "/opt/secret/app".into()],
            self_pid: 999,
            self_exe: "/opt/clipboard-monitor/clipboard-monitor",
        };
        assert!(!is_source_whitelisted(None, &ctx));
        assert!(is_source_whitelisted(
            Some(&SourceInfo {
                pid: Some(1234),
                ..Default::default()
            }),
            &ctx
        ));
        assert!(is_source_whitelisted(
            Some(&SourceInfo {
                app_name: "secretapp".into(),
                ..Default::default()
            }),
            &ctx
        ));
        assert!(is_source_whitelisted(
            Some(&SourceInfo {
                exec_path: "/opt/secret/app".into(),
                ..Default::default()
            }),
            &ctx
        ));
        // 自身 PID 恒定排除。
        assert!(is_source_whitelisted(
            Some(&SourceInfo {
                pid: Some(999),
                ..Default::default()
            }),
            &ctx
        ));
        assert!(!is_source_whitelisted(
            Some(&SourceInfo {
                pid: Some(4321),
                app_name: "normal".into(),
                ..Default::default()
            }),
            &ctx
        ));
    }

    #[test]
    fn source_and_trigger_report_when_they_carry_data() {
        assert!(!SourceInfo::default().has_any());
        assert!(SourceInfo {
            pid: Some(1),
            ..Default::default()
        }
        .has_any());
        assert!(!TriggerInfo::default().has_any());
        assert!(TriggerInfo {
            app_name: "Finder".into(),
            ..Default::default()
        }
        .has_any());
    }
}
