//! Shared between the bridge and the provider processes: the real work a winning provider
//! performs (extractive summary, review report) and the signed-result wire format.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const MAX_TEXT_CHARS: usize = 2000;
const DOMAIN: &[u8] = b"cads-auction-demo-job-v1";

const ABBREVIATIONS: &[&str] = &[
    "bzw", "ca", "dr", "etc", "evtl", "ggf", "inkl", "mio", "mr", "mrd", "mrs", "ms", "nr", "prof", "st", "str", "tel", "usw", "vgl", "vs", "zb",
];
const MONTHS: &[&str] = &[
    "januar", "februar", "märz", "april", "mai", "juni", "juli", "august", "september", "oktober", "november", "dezember",
];

/// Is the '.' at `i` part of an abbreviation or date ("Dr. Meier", "z. B.", "3. Oktober") instead
/// of the end of a sentence?
fn dot_continues_sentence(chars: &[char], i: usize) -> bool {
    let start = chars[..i].iter().rposition(|c| !c.is_alphanumeric()).map_or(0, |p| p + 1);
    let token: String = chars[start..i].iter().collect::<String>().to_lowercase();
    let next: String = chars[i + 1..].iter().skip_while(|c| c.is_whitespace()).take_while(|c| c.is_alphabetic()).collect();
    let next_lower = next.to_lowercase();
    if next.chars().next().is_some_and(char::is_lowercase) {
        return true;
    }
    let single_letter = token.chars().count() == 1 && token.chars().all(char::is_alphabetic);
    let ordinal_date = !token.is_empty() && token.len() <= 2 && token.chars().all(|c| c.is_ascii_digit()) && MONTHS.contains(&next_lower.as_str());
    single_letter || ordinal_date || ABBREVIATIONS.contains(&token.as_str())
}

/// Split into sentences on . ! ? followed by whitespace or end.
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        cur.push(*c);
        let ends = match c {
            '!' | '?' => true,
            '.' => !dot_continues_sentence(&chars, i),
            _ => false,
        };
        if ends && chars.get(i + 1).map_or(true, |n| n.is_whitespace()) {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        }
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// Extractive summary: the (up to) two sentences with the highest content-word frequency
/// score, in original order. Deterministic, language-agnostic.
pub fn summarize(text: &str) -> Value {
    let sents = sentences(text);
    let mut freq: HashMap<String, f64> = HashMap::new();
    for w in words(text).into_iter().filter(|w| w.chars().count() >= 4) {
        *freq.entry(w).or_insert(0.0) += 1.0;
    }
    let mut scored: Vec<(usize, f64)> = sents
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let ws: Vec<String> = words(s);
            let sum: f64 = ws.iter().map(|w| freq.get(w).copied().unwrap_or(0.0)).sum();
            (i, if ws.is_empty() { 0.0 } else { sum / (ws.len() as f64).sqrt() })
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    let mut keep: Vec<usize> = scored.iter().take(2).map(|x| x.0).collect();
    keep.sort();
    let summary = keep.iter().map(|&i| sents[i].as_str()).collect::<Vec<_>>().join(" ");
    json!({
        "summary": summary,
        "sentences_in": sents.len(),
        "words_in": words(text).len(),
        "words_out": words(&summary).len(),
    })
}

fn numbers(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .map(|t| t.trim_matches(|c| c == '.' || c == ',').to_string())
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .collect();
    v.dedup();
    v
}

/// Review report comparing the summary with the original text.
pub fn review(original: &str, summary: &str) -> Value {
    let win = words(original).len();
    let wout = words(summary).len();
    let reduction = if win == 0 { 0 } else { 100usize.saturating_sub(wout * 100 / win) };
    // Whole numbers, not substrings: "20" is not kept just because the summary says "2030".
    let kept = numbers(summary);
    let missing: Vec<String> = numbers(original).into_iter().filter(|n| !kept.contains(n)).collect();
    let longest = sentences(summary).iter().map(|s| words(s).len()).max().unwrap_or(0);
    let shorter_ok = wout < win;
    let long_ok = longest <= 30;
    json!({
        "reduction_pct": reduction,
        "shorter_ok": shorter_ok,
        "missing_numbers": missing,
        "numbers_ok": missing.is_empty(),
        "longest_sentence_words": longest,
        "sentence_length_ok": long_ok,
        "verdict": if shorter_ok && long_ok && missing.is_empty() { "ok" } else { "hint" },
    })
}

fn digest(job_id: &str, role: &str, output: &Value) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update((job_id.len() as u32).to_be_bytes());
    h.update(job_id.as_bytes());
    h.update((role.len() as u32).to_be_bytes());
    h.update(role.as_bytes());
    h.update(output.to_string().as_bytes());
    h.finalize().into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobReq {
    pub job_id: String,
    pub text: String,
    /// Only for the reviewer role: the summary to check against `text`.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobResp {
    pub output: Value,
    pub sig_hex: String,
    pub ms: f64,
}

pub fn sign_result(sk: &SigningKey, job_id: &str, role: &str, output: &Value) -> String {
    hex(&sk.sign(&digest(job_id, role, output)).to_bytes())
}

pub fn verify_result(pubkey: &[u8; 32], job_id: &str, role: &str, output: &Value, sig_hex: &str) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pubkey) else { return false };
    let Some(sig_bytes) = unhex(sig_hex) else { return false };
    let Ok(arr): Result<[u8; 64], _> = sig_bytes.try_into() else { return false };
    vk.verify(&digest(job_id, role, output), &Signature::from_bytes(&arr)).is_ok()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: &str = "Die Stadt baut 1200 neue Wohnungen. Die Wohnungen entstehen bis 2030 am Hafen. Das Wetter war gestern schoen. Die Stadt investiert 45 Millionen Euro in Wohnungen.";

    #[test]
    fn summary_is_extractive_and_shorter() {
        let o = summarize(T);
        let s = o["summary"].as_str().unwrap();
        assert!(o["words_out"].as_u64() < o["words_in"].as_u64());
        for sent in sentences(s) {
            assert!(T.contains(&sent), "summary sentence must be verbatim from the source");
        }
        assert!(!s.contains("Wetter"), "low-information sentence dropped");
    }

    #[test]
    fn review_flags_missing_numbers() {
        let r = review(T, "Die Stadt baut 1200 neue Wohnungen.");
        assert_eq!(r["verdict"], "hint");
        let miss = r["missing_numbers"].as_array().unwrap();
        assert!(miss.iter().any(|m| m == "2030") && miss.iter().any(|m| m == "45"));
        assert!(!miss.iter().any(|m| m == "1200"));
    }

    /// Live on 2026-10-30: the review said "all numbers kept" with the 20 from the original missing.
    #[test]
    fn a_number_is_not_kept_because_a_longer_one_contains_it() {
        let r = review("Die Stadt baut 2030 neu. Kurz: 20 Tage.", "Die Stadt baut 2030 neu.");
        assert_eq!(r["numbers_ok"], false);
        assert_eq!(r["missing_numbers"], json!(["20"]));
    }

    /// Live on 2026-10-30: this text came back as "... Das Projekt kostet ca." with 10 "sentences".
    #[test]
    fn abbreviations_and_dates_do_not_end_a_sentence() {
        let t = "Dr. Meier und Prof. Schulz leiten das Projekt z. B. am 3. Oktober in Hamburg. Das Projekt kostet 1.200 Euro. Es kostet ca. 3,5 Mio. Euro insgesamt.";
        let s = sentences(t);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[2].ends_with("Euro insgesamt."));
        assert_eq!(sentences("Der Bau endet 2030. Danach folgt mehr.").len(), 2, "a year ends a sentence");
    }

    #[test]
    fn signature_binds_job_role_and_output() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let out = json!({"a": 1});
        let sig = sign_result(&sk, "j1", "writer", &out);
        assert!(verify_result(&pk, "j1", "writer", &out, &sig));
        assert!(!verify_result(&pk, "j2", "writer", &out, &sig));
        assert!(!verify_result(&pk, "j1", "reviewer", &out, &sig));
        assert!(!verify_result(&pk, "j1", "writer", &json!({"a": 2}), &sig));
        assert!(!verify_result(&pk, "j1", "writer", &out, "zz"));
    }

    #[test]
    fn short_text_edge_cases() {
        assert_eq!(summarize("")["summary"], "");
        assert_eq!(summarize("Nur ein Satz")["summary"], "Nur ein Satz");
    }
}
