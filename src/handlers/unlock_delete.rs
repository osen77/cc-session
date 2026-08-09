//! CLI handler for `ccs unlock-delete`.

use crate::sync::delete_unlock;
use anyhow::Result;
use colored::Colorize;

/// Handle `ccs unlock-delete`.
/// `off` and `status` take priority over opening a window.
pub fn handle_unlock_delete(minutes: u64, off: bool, status: bool) -> Result<()> {
    if off {
        delete_unlock::disable()?;
        println!("{} 删除放行窗口已关闭，恢复保护模式。", "✓".green());
        return Ok(());
    }

    if status {
        match delete_unlock::status()? {
            Some(secs) => println!(
                "{} 删除放行窗口生效中，剩余约 {} 分钟。",
                "🔓".yellow(),
                secs / 60
            ),
            None => println!("{} 当前处于保护状态（删除不会同步到云端）。", "🔒".green()),
        }
        return Ok(());
    }

    if minutes == 0 {
        anyhow::bail!("时长必须 ≥ 1 分钟；如需关闭请用 `ccs unlock-delete --off`");
    }

    let expires_at = delete_unlock::unlock(minutes)?;
    let expire_local = chrono::DateTime::from_timestamp(expires_at as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string());

    println!(
        "{} 已开启删除放行窗口 {} 分钟（到期 {}）。",
        "🔓".yellow(),
        minutes,
        expire_local
    );
    println!(
        "  {} 窗口期内 push（含自动同步）会把本地已删除的 session 同步删除到云端，请谨慎。",
        "⚠".yellow()
    );
    Ok(())
}
