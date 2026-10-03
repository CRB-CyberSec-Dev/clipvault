//! Capture filtering: size caps and sensitive-source heuristics.
//!
//! X11 has no standard "this is a password" marker. We honor the
//! `x-kde.passwordManagerHint` target convention (Klipper/KeePassXC) and a
//! configurable source-app blocklist. Residual risk is documented in README.

use crate::config::Config;
use crate::types::NewClip;

/// The MIME/target name password managers offering KDE compatibility set
/// alongside their data. Its mere presence means "do not store".
pub const PASSWORD_HINT_TARGET: &str = "x-kde.passwordManagerHint";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Capture,
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    PasswordHint,
    ExcludedApp(String),
    TooLarge { size: u64, max: u64 },
    ImagesDisabled,
    Empty,
}

/// Decide whether a freshly observed clip should be stored.
///
/// `targets` is the list of MIME/target names the owner offered — the
/// password hint can only be detected there, before reading any data.
pub fn judge(clip: &NewClip, targets: &[String], cfg: &Config) -> Verdict {
    if cfg.exclude_password_managers
        && targets.iter().any(|t| t == PASSWORD_HINT_TARGET)
    {
        return Verdict::Skip(SkipReason::PasswordHint);
    }

    if let Some(app) = &clip.source_app {
        let app_lc = app.to_lowercase();
        if let Some(hit) = cfg
            .excluded_apps
            .iter()
            .find(|pat| !pat.is_empty() && app_lc.contains(&pat.to_lowercase()))
        {
            return Verdict::Skip(SkipReason::ExcludedApp(hit.clone()));
        }
    }

    if matches!(clip.kind, crate::types::ClipKind::Image) && !cfg.monitor_images {
        return Verdict::Skip(SkipReason::ImagesDisabled);
    }

    if clip.byte_size > cfg.max_item_bytes {
        return Verdict::Skip(SkipReason::TooLarge {
            size: clip.byte_size,
            max: cfg.max_item_bytes,
        });
    }

    if clip.byte_size == 0 {
        return Verdict::Skip(SkipReason::Empty);
    }

    Verdict::Capture
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ClipKind;

    fn text_clip(s: &str) -> NewClip {
        NewClip {
            kind: ClipKind::Text,
            text_content: Some(s.into()),
            html_content: None,
            image_png: None,
            byte_size: s.len() as u64,
            source_app: None,
        }
    }

    #[test]
    fn normal_text_captured() {
        let cfg = Config::default();
        assert_eq!(
            judge(&text_clip("hello"), &["UTF8_STRING".into()], &cfg),
            Verdict::Capture
        );
    }

    #[test]
    fn password_hint_target_skipped() {
        let cfg = Config::default();
        let targets = vec!["UTF8_STRING".into(), PASSWORD_HINT_TARGET.into()];
        assert_eq!(
            judge(&text_clip("s3cret"), &targets, &cfg),
            Verdict::Skip(SkipReason::PasswordHint)
        );
    }

    #[test]
    fn excluded_app_skipped_case_insensitive() {
        let cfg = Config::default();
        let mut clip = text_clip("s3cret");
        clip.source_app = Some("KeePassXC".into());
        assert!(matches!(
            judge(&clip, &[], &cfg),
            Verdict::Skip(SkipReason::ExcludedApp(_))
        ));
    }

    #[test]
    fn oversize_skipped_with_numbers() {
        let mut cfg = Config::default();
        cfg.max_item_bytes = 10;
        let clip = text_clip("this is longer than ten bytes");
        assert_eq!(
            judge(&clip, &[], &cfg),
            Verdict::Skip(SkipReason::TooLarge { size: 29, max: 10 })
        );
    }

    #[test]
    fn images_respect_toggle() {
        let mut cfg = Config::default();
        cfg.monitor_images = false;
        let clip = NewClip {
            kind: ClipKind::Image,
            text_content: None,
            html_content: None,
            image_png: Some(vec![0u8; 100]),
            byte_size: 100,
            source_app: None,
        };
        assert_eq!(
            judge(&clip, &["image/png".into()], &cfg),
            Verdict::Skip(SkipReason::ImagesDisabled)
        );
    }

    #[test]
    fn empty_skipped() {
        let cfg = Config::default();
        assert_eq!(
            judge(&text_clip(""), &[], &cfg),
            Verdict::Skip(SkipReason::Empty)
        );
    }
}
