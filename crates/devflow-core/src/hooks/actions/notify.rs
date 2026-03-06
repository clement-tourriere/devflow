use anyhow::Result;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::{HookContext, NotifyLevel};

/// Send a desktop notification.
pub fn execute(
    title_template: &str,
    message_template: &str,
    level: &NotifyLevel,
    context: &HookContext,
    template_engine: &TemplateEngine,
) -> Result<ActionResult> {
    let title = template_engine.render(title_template, context)?;
    let message = template_engine.render(message_template, context)?;

    send_notification(&title, &message, level);

    Ok(ActionResult {
        summary: format!("notify: {} ({})", title, level_str(level)),
    })
}

fn level_str(level: &NotifyLevel) -> &'static str {
    match level {
        NotifyLevel::Info => "info",
        NotifyLevel::Success => "success",
        NotifyLevel::Warning => "warning",
        NotifyLevel::Error => "error",
    }
}

fn send_notification(title: &str, message: &str, _level: &NotifyLevel) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            message.replace('"', "\\\""),
            title.replace('"', "\\\"")
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .output();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, message])
            .output();
    }

    #[cfg(target_os = "windows")]
    {
        // PowerShell toast notification
        let script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
             $template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $textNodes = $template.GetElementsByTagName('text'); \
             $textNodes.Item(0).AppendChild($template.CreateTextNode('{}')) > $null; \
             $textNodes.Item(1).AppendChild($template.CreateTextNode('{}')) > $null; \
             $toast = [Windows.UI.Notifications.ToastNotification]::new($template); \
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('devflow').Show($toast)",
            title.replace('\'', "''"),
            message.replace('\'', "''")
        );
        let _ = std::process::Command::new("powershell")
            .args(["-Command", &script])
            .output();
    }
}
