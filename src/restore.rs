//! 把历史记录重新写回系统剪贴板（"恢复到剪贴板"）。
//!
//! Windows/macOS 用 arboard 的 `set_text` / `set_image`；X11/Wayland 下
//! arboard 会在 `Clipboard::new()` 时启动一个后台线程持续响应
//! `SelectionRequest`（X11）或 data-source 请求（Wayland），因此这里必须把
//! `Clipboard` 实例**长期持有**，不能每次 restore 都新建后立刻丢弃 —— 否则
//! 剪贴板所有权随对象析构立刻失效，粘贴时拿不到内容。
//!
//! 图片以 RGBA 形式回写；只有 `contents.body` 有原文的文本才回写文本，
//! 被截断/只有摘要的记录明确拒绝，避免把不完整内容写回剪贴板。

use anyhow::{bail, Context, Result};
use arboard::{Clipboard, ImageData};
use std::borrow::Cow;
use std::sync::Mutex;

/// 长期持有剪贴板上下文的恢复器。`App` 持有它，进程生命周期内不 drop。
pub struct RestoreManager {
    clipboard: Mutex<Option<Clipboard>>,
    /// 初始化失败原因；延迟初始化，失败不阻断进程启动。
    error: Mutex<Option<String>>,
}

impl Default for RestoreManager {
    fn default() -> Self {
        Self {
            clipboard: Mutex::new(None),
            error: Mutex::new(None),
        }
    }
}

impl RestoreManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_clipboard(&self) -> Result<()> {
        let mut guard = self.clipboard.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        match Clipboard::new() {
            Ok(c) => {
                *guard = Some(c);
                Ok(())
            }
            Err(err) => {
                let message = format!("初始化系统剪贴板失败: {err}");
                *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
                bail!("{message}")
            }
        }
    }

    /// 恢复文本。会拒绝空文本。
    pub fn restore_text(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            bail!("这条记录没有可恢复的文本正文");
        }
        self.ensure_clipboard()?;
        let mut guard = self.clipboard.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = guard.as_mut().context("剪贴板未初始化")?;
        clipboard
            .set_text(text.to_owned())
            .map_err(|e| anyhow::anyhow!("写入剪贴板失败: {e}"))
    }

    /// 恢复图片（RGBA 像素）。
    pub fn restore_image(&self, width: usize, height: usize, rgba: Vec<u8>) -> Result<()> {
        if width == 0 || height == 0 || rgba.len() != width * height * 4 {
            bail!("图片尺寸或像素数据不完整");
        }
        self.ensure_clipboard()?;
        let mut guard = self.clipboard.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = guard.as_mut().context("剪贴板未初始化")?;
        clipboard
            .set_image(ImageData {
                width,
                height,
                bytes: Cow::Owned(rgba),
            })
            .map_err(|e| anyhow::anyhow!("写入图片到剪贴板失败: {e}"))
    }

    /// 最近一次初始化错误（供 /api/status 展示）。
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 从事件详情判断可以恢复成什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restorable {
    Text(String),
    /// (width, height, rgba)
    Image(usize, usize, Vec<u8>),
    None,
}

/// 解析 `EventDetail` 的 body/meta_json，决定可恢复内容。
/// 纯函数，便于单测；不触碰系统剪贴板。
pub fn parse_restorable(
    body: Option<&str>,
    meta_json: Option<&str>,
    content_type: &str,
    truncated: bool,
    has_blob: bool,
) -> Restorable {
    if truncated && body.is_none() {
        return Restorable::None;
    }
    if let Some(text) = body {
        if !text.is_empty() {
            return Restorable::Text(text.to_string());
        }
    }
    // 图片：本项目从不保存像素，只能回写占位图并明确拒绝。
    let _ = (meta_json, content_type, has_blob);
    Restorable::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_body_is_restorable() {
        assert_eq!(
            parse_restorable(Some("hello 世界"), Some("{}"), "text", false, false),
            Restorable::Text("hello 世界".into())
        );
    }

    #[test]
    fn empty_or_truncated_content_is_not_restorable() {
        assert_eq!(
            parse_restorable(None, Some("{}"), "text", true, false),
            Restorable::None
        );
        assert_eq!(
            parse_restorable(Some(""), Some("{}"), "text", false, false),
            Restorable::None
        );
        assert_eq!(
            parse_restorable(None, Some(r#"{"image":{"width":2}}"#), "image", false, true),
            Restorable::None,
            "本项目不保存像素，图片不能静默回写错误内容"
        );
    }

    #[test]
    fn restore_text_rejects_empty() {
        // 不触碰系统剪贴板，只验证参数校验分支。
        let manager = RestoreManager::new();
        assert!(manager.restore_text("").is_err());
    }
}
