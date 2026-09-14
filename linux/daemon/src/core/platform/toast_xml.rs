//! The toast XML document a Windows notification is built from.
//!
//! # Why this is not inside the Windows module
//!
//! The dialect is Windows-only, but the code is pure string building — and it
//! is the one part of the notification path that handles text this machine did
//! not author. A mirrored phone notification's title and body go straight in
//! here, so a missing escape is not cosmetic: `&` or `<` in a message makes the
//! document unparseable and the notification silently vanishes, and text that
//! closes an element early can inject its own `<action>` buttons into a prompt
//! the user is about to trust.
//!
//! Compiled on every platform so it can be tested on the machine this is
//! developed on, rather than being verified for the first time on Windows.

/// Build the toast XML for a notification.
///
/// `actions` is `(key, label)`; the key comes back as the activation argument,
/// so it is what the `fc:` / `call:` / `act:` consumers filter on.
pub fn toast_xml(summary: &str, body: &str, actions: &[(String, String)], urgent: bool) -> String {
    let mut xml = String::from("<toast");
    if urgent {
        // Keeps the banner up until acted on, instead of the default few
        // seconds — a call banner or a file-consent prompt that vanishes on its
        // own is worse than none, because the user answers a question they
        // never saw.
        xml.push_str(" scenario=\"urgent\"");
    }
    xml.push_str("><visual><binding template=\"ToastGeneric\">");
    xml.push_str("<text>");
    xml.push_str(&escape(summary));
    xml.push_str("</text>");
    if !body.is_empty() {
        xml.push_str("<text>");
        xml.push_str(&escape(body));
        xml.push_str("</text>");
    }
    xml.push_str("</binding></visual>");
    if !actions.is_empty() {
        xml.push_str("<actions>");
        for (key, label) in actions {
            xml.push_str("<action content=\"");
            xml.push_str(&escape(label));
            xml.push_str("\" arguments=\"");
            xml.push_str(&escape(key));
            xml.push_str("\" activationType=\"foreground\"/>");
        }
        xml.push_str("</actions>");
    }
    xml.push_str("</toast>");
    xml
}

/// Escape the five predefined XML entities.
///
/// Ampersand first, and only once: doing it in any other order would rewrite
/// the `&` of an escape produced by an earlier replacement, turning `<` into
/// `&amp;lt;` and showing the user the escape instead of the character.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_decline() -> Vec<(String, String)> {
        vec![
            ("fc:accept".to_string(), "Accept".to_string()),
            ("fc:decline".to_string(), "Decline".to_string()),
        ]
    }

    #[test]
    fn escapes_all_five_entities_ampersand_first() {
        assert_eq!(escape("a&b"), "a&amp;b");
        assert_eq!(escape("<x>"), "&lt;x&gt;");
        assert_eq!(escape("\"q\" 'a'"), "&quot;q&quot; &apos;a&apos;");
        // The ordering trap: an ampersand introduced by escaping `<` must not
        // be escaped again.
        assert_eq!(escape("<"), "&lt;");
        assert!(!escape("<").contains("&amp;"));
    }

    /// The attack this file exists to prevent: a phone notification whose text
    /// tries to close the document and add its own button to a consent prompt.
    #[test]
    fn remote_text_cannot_inject_an_action_button() {
        let hostile = "hi</text></binding></visual><actions>\
                       <action content=\"Allow\" arguments=\"fc:accept\"/></actions><!--";
        let xml = toast_xml("Phone", hostile, &[], false);
        // Exactly zero real action elements: we passed none.
        assert_eq!(xml.matches("<action ").count(), 0);
        // The attempt survives as visible text, escaped.
        assert!(xml.contains("&lt;action content=&quot;Allow&quot;"));
        assert!(!xml.contains("<actions>"));
    }

    /// A hostile body must not be able to add a button to a prompt that DOES
    /// have buttons either — the dangerous case, since the user is already
    /// being asked to click something.
    #[test]
    fn injection_cannot_add_a_button_to_a_real_prompt() {
        let hostile = "</text><action content=\"Yes\" arguments=\"fc:accept\"/>";
        let xml = toast_xml("Phone wants to send a file", hostile, &accept_decline(), true);
        assert_eq!(
            xml.matches("<action ").count(),
            2,
            "only the two buttons we asked for"
        );
        assert!(xml.contains("arguments=\"fc:accept\" activationType"));
        assert!(xml.contains("&lt;action content=&quot;Yes&quot;"));
    }

    #[test]
    fn urgent_sets_the_scenario_and_plain_does_not() {
        assert!(toast_xml("s", "b", &[], true).starts_with("<toast scenario=\"urgent\">"));
        assert!(toast_xml("s", "b", &[], false).starts_with("<toast>"));
    }

    #[test]
    fn an_empty_body_omits_its_element() {
        let xml = toast_xml("only a title", "", &[], false);
        assert_eq!(xml.matches("<text>").count(), 1);
    }

    /// Every action reaches the document with its key intact — that key is what
    /// the consumers dispatch on, so a mangled one is a dead button.
    #[test]
    fn action_keys_and_labels_both_survive() {
        let xml = toast_xml("Call", "Alice", &accept_decline(), true);
        assert!(xml.contains("content=\"Accept\" arguments=\"fc:accept\""));
        assert!(xml.contains("content=\"Decline\" arguments=\"fc:decline\""));
    }

    /// Tags are balanced and the payload sits inside them — a cheap structural
    /// check that catches a builder edit that drops a closing tag.
    #[test]
    fn the_document_is_balanced() {
        let xml = toast_xml("t", "b", &accept_decline(), false);
        for tag in ["toast", "visual", "binding", "actions"] {
            assert_eq!(
                xml.matches(&format!("<{tag}")).count(),
                xml.matches(&format!("</{tag}>")).count(),
                "unbalanced <{tag}>"
            );
        }
        assert!(xml.ends_with("</toast>"));
    }
}
