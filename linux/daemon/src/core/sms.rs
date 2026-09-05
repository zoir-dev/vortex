//! SMS mirror (phone → laptop companion Messages page).
//!
//! Same chunked transfer as contacts/call-log: the phone sends its recent SMS
//! as a JSON array split across `ty::SMS` frames; we reassemble the single
//! stream and the UI layer caches it (`~/.cache/vortex/sms.json`) + groups it
//! into conversations. Bodies are sensitive — never logged. Mirrors Kotlin
//! `core::sms::SmsMessage`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmsMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub body: String,
    /// `Telephony.Sms.TYPE`: 1=inbox (received), 2=sent.
    #[serde(default)]
    pub r#type: i32,
    /// Epoch milliseconds.
    #[serde(default)]
    pub date: i64,
    /// Conversation thread id.
    #[serde(default)]
    pub thread: i64,
    /// `Telephony.Sms.READ`: 0=unread, 1=read.
    #[serde(default)]
    pub read: i32,
}

/// Parse an SMS frame plaintext: `[total u16 BE][idx u16 BE][json-chunk]`.
pub fn parse_chunk(plain: &[u8]) -> Option<(u16, u16, Vec<u8>)> {
    if plain.len() < 4 {
        return None;
    }
    let total = u16::from_be_bytes([plain[0], plain[1]]);
    let idx = u16::from_be_bytes([plain[2], plain[3]]);
    Some((total, idx, plain[4..].to_vec()))
}

/// Upper bound on declared chunk counts — same rationale as
/// `contacts::MAX_CHUNKS`: the recent-SMS list is well under a hundred
/// 450-byte chunks, so a larger declared total is hostile/corrupt and must
/// not drive the buffer allocation.
pub const MAX_CHUNKS: u16 = 2048;

/// Reassembles the single SMS JSON stream from its chunks. Returns the full JSON
/// bytes once every chunk has arrived. A re-send with a different chunk count
/// restarts the buffer.
#[derive(Default)]
pub struct SmsAssembler {
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
}

impl SmsAssembler {
    pub fn add(&mut self, total: u16, idx: u16, data: Vec<u8>) -> Option<Vec<u8>> {
        if total == 0 || total > MAX_CHUNKS || idx >= total {
            return None;
        }
        if self.total != total {
            self.total = total;
            self.chunks = vec![None; total as usize];
        }
        self.chunks[idx as usize] = Some(data);
        if self.chunks.iter().any(|c| c.is_none()) {
            return None;
        }
        let mut bytes = Vec::new();
        for c in &self.chunks {
            bytes.extend_from_slice(c.as_ref().unwrap());
        }
        self.total = 0;
        self.chunks = Vec::new();
        Some(bytes)
    }
}

/// Shortest and longest digit run we will treat as a login code.
const OTP_MIN: usize = 4;
const OTP_MAX: usize = 8;

/// How far from a digit run a hint word still counts as describing it.
const OTP_HINT_WINDOW: usize = 48;

/// Words that mark a nearby number as a login code. English, Uzbek and Russian
/// together, because a phone here receives all three — often in one message.
const OTP_HINTS: &[&str] = &[
    "code", "otp", "pin", "password", "verification", "verify", "one-time",
    "kod", "kodi", "kodingiz", "parol", "tasdiq", "bir martalik",
    "код", "коды", "пароль", "подтверж", "одноразов",
];

/// Pull a one-time login code out of an SMS body, if it plausibly has one.
///
/// Deliberately conservative: a hint word must appear somewhere in the message,
/// because plenty of ordinary texts (prices, order numbers, balances) contain a
/// bare four-to-eight digit number and copying one of those over the user's
/// clipboard is worse than missing a code. Among the candidates, the one
/// closest to a hint word wins, so "purchase 50000 sum, code 1234" picks 1234.
///
/// Digit runs glued to a `+`, to a letter, or to a grouping separator with more
/// digits on its far side are skipped — that is what phone numbers, IDs, money
/// amounts, dates and times look like.
pub fn extract_otp(body: &str) -> Option<String> {
    let lower = body.to_lowercase();
    // Spans, not just positions: which END of the hint faces the number decides
    // how close it really is, and "code: 1234" is a much stronger claim than a
    // word that merely happens to sit nearby.
    // Whole words only. `match_indices` matched substrings, so "pin" fired
    // inside "shipping", "code" inside "barcode" and "postcode", "kod" inside
    // "Kodak" — and one of those anywhere in the body qualified every 4-8 digit
    // run in it. "Your order 48213 is shipping" put 48213 on the clipboard and
    // announced it as a login code.
    let is_word_char = |c: char| c.is_alphanumeric();
    let hints: Vec<(usize, usize)> = OTP_HINTS
        .iter()
        .flat_map(|h| lower.match_indices(h).map(|(i, m)| (i, i + m.len())))
        .filter(|&(start, end)| {
            let before_ok = lower[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
            let after_ok = lower[end..].chars().next().is_none_or(|c| !is_word_char(c));
            before_ok && after_ok
        })
        .collect();
    if hints.is_empty() {
        return None;
    }

    let b = body.as_bytes();
    // ASCII digits never appear inside a multi-byte UTF-8 sequence, so plain
    // byte scanning is safe here even for Cyrillic bodies.
    let is_sep = |c: u8| matches!(c, b'.' | b',' | b':' | b'-' | b'/');
    let mut best: Option<(i32, &str)> = None;
    let mut i = 0usize;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let (end, len) = (i, i - start);
        if !(OTP_MIN..=OTP_MAX).contains(&len) {
            continue;
        }
        let prev = start.checked_sub(1).map(|p| b[p]);
        let next = b.get(end).copied();
        // Phone number, or part of a longer identifier.
        if prev == Some(b'+')
            || prev.is_some_and(|c| c.is_ascii_alphabetic())
            || next.is_some_and(|c| c.is_ascii_alphabetic())
        {
            continue;
        }
        // "1.234", "12:34", "2026-07-28": a separator with digits beyond it
        // means this run is one field of a bigger number, not a code.
        if prev.is_some_and(is_sep) && start >= 2 && b[start - 2].is_ascii_digit() {
            continue;
        }
        if next.is_some_and(is_sep) && b.get(end + 1).is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }

        // A hint BEFORE the number ("code: 1234") is the dominant shape and the
        // stronger signal; one after ("1234 is your code") is real but weaker.
        // Without that asymmetry a hint word trailing an unrelated number wins
        // on raw proximity — "purchase 50000 sum. Confirmation code 1234".
        let mut proximity = 0;
        for &(hs, he) in &hints {
            let (distance, weight) = if he <= start {
                (start - he, 14)
            } else if hs >= end {
                (hs - end, 8)
            } else {
                (0, 14)
            };
            if distance <= OTP_HINT_WINDOW {
                proximity =
                    proximity.max(weight + (OTP_HINT_WINDOW - distance) as i32 / 4);
            }
        }
        let mut score = match len {
            6 => 3,
            5 => 2,
            4 | 7 | 8 => 1,
            _ => 0,
        };
        score += proximity;
        // A hint that is nowhere near this number is not evidence about it.
        // `proximity` stays 0 past the window, and the length bonus alone used
        // to be enough to elect a candidate — so a hint at one end of a long
        // message crowned an unrelated number at the other.
        if proximity == 0 {
            continue;
        }
        let candidate = &body[start..end];
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, c)| c.to_string())
}

#[cfg(test)]
mod otp_word_boundary_tests {
    use super::extract_otp;

    #[test]
    fn hint_words_inside_other_words_do_not_count() {
        // "shipping" contains "pin"; "barcode" and "postcode" contain "code";
        // "Kodak" contains "kod". None of these is a login code.
        for body in [
            "Your order 48213 is shipping",
            "Scan barcode 99187 at the till",
            "Delivery to postcode 10025 confirmed",
            "Kodak order 55512 is ready",
        ] {
            assert_eq!(extract_otp(body), None, "must not treat as an OTP: {body}");
        }
    }

    #[test]
    fn a_real_code_is_still_found() {
        assert_eq!(extract_otp("Your code is 481923"), Some("481923".to_string()));
        assert_eq!(extract_otp("123456 is your verification code"), Some("123456".to_string()));
    }

    #[test]
    fn a_far_away_hint_does_not_elect_an_unrelated_number() {
        let body = "Payment of 500000 sum accepted. Thank you for shopping with us \
                    today, and remember you can reach support any time. Reference code";
        assert_eq!(extract_otp(body), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_codes_in_the_three_languages_a_phone_here_receives() {
        assert_eq!(extract_otp("Your code is 483920").as_deref(), Some("483920"));
        assert_eq!(
            extract_otp("Tasdiqlash kodi: 4821").as_deref(),
            Some("4821")
        );
        assert_eq!(
            extract_otp("Ваш код подтверждения: 90210").as_deref(),
            Some("90210")
        );
        // Hint after the number.
        assert_eq!(
            extract_otp("123456 is your verification code").as_deref(),
            Some("123456")
        );
    }

    #[test]
    fn picks_the_number_the_hint_word_describes() {
        assert_eq!(
            extract_otp("Xarid: 50000 sum. Tasdiqlash kodi 1234").as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn ignores_messages_with_no_hint_word() {
        assert_eq!(extract_otp("Balansingiz 45000 so'm"), None);
        assert_eq!(extract_otp("See you at 1830"), None);
    }

    #[test]
    fn skips_phone_numbers_amounts_dates_and_ids() {
        // A hint word is present, but every number is something else.
        assert_eq!(extract_otp("Kod uchun qo'ng'iroq: +998901234567"), None);
        assert_eq!(extract_otp("kod: 1.234.567"), None);
        assert_eq!(extract_otp("kodi 2026-07-28"), None);
        assert_eq!(extract_otp("kod ID4821X"), None);
        // 16 digits is a card, not a code.
        assert_eq!(extract_otp("kod 8600123412341234"), None);
    }

    #[test]
    fn rejects_total_above_cap() {
        let mut asm = SmsAssembler::default();
        assert!(asm.add(MAX_CHUNKS + 1, 0, b"x".to_vec()).is_none());
        assert_eq!(asm.add(1, 0, b"ok".to_vec()), Some(b"ok".to_vec()));
    }
}
