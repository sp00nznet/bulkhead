//! The piece of paper.
//!
//! Half of what Blancco and KillDisk actually sell is this: a record that
//! says which drive was erased, by which command, and what was checked
//! afterwards. The erase is a command the drive already implements; the
//! certificate is the deliverable an auditor keeps.
//!
//! Two forms of the same record. JSON is the one a script reads; HTML is the
//! one that prints. Nothing here decides anything -- it renders what the
//! erase observed, including the parts that did not go well, because a
//! certificate that only exists on success is a certificate that lies by
//! omission.
use crate::util::{Ctx, Res, human};

/// One verification sample: what was there before, and what is there now.
pub struct Point {
    pub at: u64,
    pub before: String,
    pub after: String,
    pub ok: bool,
}

pub struct Cert {
    pub when: String,
    pub host: String,
    pub operator: String,
    pub tool: String,
    pub disk: u32,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub bus: String,
    pub size: u64,
    pub method: String,
    pub seconds: u64,
    /// Whether the sampled points held anything before the erase. If they did
    /// not, the run proves the command succeeded, not that data was removed.
    pub had_data: bool,
    pub points: Vec<Point>,
    pub passed: bool,
    pub caveats: Vec<String>,
}

/// NIST SP 800-88 Rev. 1 sorts media sanitization into Clear, Purge and
/// Destroy. The line between the first two is exactly the remapped-block
/// problem: a host-issued overwrite reaches what the drive currently maps
/// (Clear); a firmware sanitize reaches the media itself (Purge).
pub fn standard(method: &str) -> &'static str {
    match method {
        "overwrite" => "Clear",
        "nvme-format" => "Clear",
        m if m.contains("sanitize") || m == "nvme-format-crypto" => "Purge",
        _ => "Clear",
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

pub fn utc_now() -> String {
    // ponytail: UTC, so the record is unambiguous without carrying a timezone
    // database around. Local time is the reader's problem.
    let t = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn duration(s: u64) -> String {
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// Who ran it, on what. Not identity -- just the account and machine, which is
/// what the record can honestly claim.
pub fn who() -> (String, String) {
    let host = env("COMPUTERNAME");
    let domain = env("USERDOMAIN");
    let user = env("USERNAME");
    let operator = if domain.is_empty() || domain == host {
        user
    } else {
        format!("{domain}\\{user}")
    };
    (host, operator)
}

impl Cert {
    pub fn verdict(&self) -> &'static str {
        if self.passed { "VERIFIED" } else { "FAILED" }
    }

    /// Write by extension: `.json` for the machine, anything else for print.
    pub fn write(&self, path: &str) -> Res<()> {
        let body = if path.to_ascii_lowercase().ends_with(".json") {
            self.json()
        } else {
            self.html()
        };
        std::fs::write(path, body).ctx("write certificate")?;
        Ok(())
    }

    pub fn json(&self) -> String {
        let mut s = String::from("{\n");
        let mut kv = |k: &str, v: String| {
            s.push_str(&format!("  \"{k}\": {v},\n"));
        };
        kv("tool", q(&self.tool));
        kv("when", q(&self.when));
        kv("host", q(&self.host));
        kv("operator", q(&self.operator));
        kv("disk", self.disk.to_string());
        kv("model", q(&self.model));
        kv("serial", q(&self.serial));
        kv("firmware", q(&self.firmware));
        kv("bus", q(&self.bus));
        kv("bytes", self.size.to_string());
        kv("capacity", q(&human(self.size)));
        kv("method", q(&self.method));
        kv("nist_800_88", q(standard(&self.method)));
        kv("seconds", self.seconds.to_string());
        kv("held_data_before", self.had_data.to_string());
        kv("verified", self.passed.to_string());
        kv("result", q(self.verdict()));
        s.push_str("  \"caveats\": [\n");
        for (i, c) in self.caveats.iter().enumerate() {
            s.push_str(&format!(
                "    {}{}\n",
                q(c),
                if i + 1 == self.caveats.len() { "" } else { "," }
            ));
        }
        s.push_str("  ],\n  \"samples\": [\n");
        for (i, p) in self.points.iter().enumerate() {
            s.push_str(&format!(
                "    {{\"offset\": {}, \"before\": {}, \"after\": {}, \"ok\": {}}}{}\n",
                p.at,
                q(&p.before),
                q(&p.after),
                p.ok,
                if i + 1 == self.points.len() { "" } else { "," }
            ));
        }
        s.push_str("  ]\n}\n");
        s
    }

    pub fn html(&self) -> String {
        let mut h = String::new();
        h.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
        h.push_str(
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>",
        );
        h.push_str(&e(&format!(
            "Erasure certificate - {} {}",
            self.model, self.serial
        )));
        h.push_str("</title>\n<style>\n");
        h.push_str(CSS);
        h.push_str("</style></head><body>\n<main class=\"sheet\">\n");

        // Header: what this is, which drive, and whether it worked.
        h.push_str("<header>\n<div><p class=\"eyebrow\">bulkhead</p>\n");
        h.push_str("<h1>Certificate of Data Erasure</h1>\n<p class=\"sub\">");
        h.push_str(&e(&self.model));
        h.push_str(" &middot; serial ");
        h.push_str(&e(&self.serial));
        h.push_str("</p></div>\n");
        h.push_str(&format!(
            "<div class=\"badge {}\"><span>{}</span><small>{}</small></div>\n</header>\n",
            if self.passed { "ok" } else { "bad" },
            self.verdict(),
            e(&self.when)
        ));

        // The two things being certified: the device, and what was done to it.
        h.push_str("<section class=\"cols\">\n<div><h2>Device</h2><dl>\n");
        for (k, v) in [
            ("Model", self.model.clone()),
            ("Serial", self.serial.clone()),
            ("Firmware", self.firmware.clone()),
            ("Interface", self.bus.clone()),
            (
                "Capacity",
                format!("{} ({} bytes)", human(self.size), self.size),
            ),
            ("Address", format!("disk {}", self.disk)),
        ] {
            h.push_str(&row(k, &v));
        }
        h.push_str("</dl></div>\n<div><h2>Erasure</h2><dl>\n");
        for (k, v) in [
            ("Method", self.method.clone()),
            (
                "NIST 800-88",
                format!("{} (Rev. 1)", standard(&self.method)),
            ),
            ("Completed", self.when.clone()),
            ("Elapsed", duration(self.seconds)),
            ("Tool", self.tool.clone()),
            ("Host", self.host.clone()),
            ("Operator", self.operator.clone()),
        ] {
            h.push_str(&row(k, &v));
        }
        h.push_str("</dl></div>\n</section>\n");

        // Verification: the whole point. A claim with nothing read back is a
        // claim, so the evidence gets the largest section on the page.
        h.push_str("<section>\n<h2>Verification</h2>\n<p class=\"lead\">");
        h.push_str(&e(&self.summary()));
        h.push_str("</p>\n<div class=\"map\">\n");
        for p in &self.points {
            h.push_str(&format!(
                "<i class=\"{}\" title=\"{} - {}\"></i>",
                if p.ok { "ok" } else { "bad" },
                e(&human(p.at)),
                if p.ok { "verified" } else { "not verified" }
            ));
        }
        h.push_str("\n</div>\n<p class=\"scale\"><span>start of drive</span><span>end of drive</span></p>\n");
        h.push_str("<table>\n<thead><tr><th>Offset</th><th>Before (first 16 bytes)</th>");
        h.push_str("<th>After (first 16 bytes)</th><th>Result</th></tr></thead>\n<tbody>\n");
        for p in &self.points {
            h.push_str(&format!(
                "<tr><td>{}</td><td class=\"hex\">{}</td><td class=\"hex\">{}</td><td class=\"{}\">{}</td></tr>\n",
                e(&human(p.at)),
                e(&p.before),
                e(&p.after),
                if p.ok { "pass" } else { "fail" },
                if p.ok { "pass" } else { "FAIL" }
            ));
        }
        h.push_str("</tbody></table>\n</section>\n");

        // What this does not prove. Printed on the certificate itself, not
        // filed away in a manual, because that is where it gets read.
        h.push_str("<section class=\"limits\">\n<h2>What this does not prove</h2>\n<ul>\n");
        for c in &self.caveats {
            h.push_str(&format!("<li>{}</li>\n", e(c)));
        }
        h.push_str("</ul>\n</section>\n");

        h.push_str(
            "<footer>\n<div class=\"sign\"><span></span><small>Operator signature</small></div>\n",
        );
        h.push_str("<div class=\"sign\"><span></span><small>Date</small></div>\n</footer>\n");
        h.push_str("<p class=\"fine\">Generated by ");
        h.push_str(&e(&self.tool));
        h.push_str(
            ". This is a record of what the tool observed on the host named above. \
                    It is self-attested and carries no cryptographic signature or third-party \
                    validation.</p>\n",
        );
        h.push_str("</main>\n</body></html>\n");
        h
    }

    fn summary(&self) -> String {
        let n = self.points.len();
        let bad = self.points.iter().filter(|p| !p.ok).count();
        if bad > 0 {
            return format!(
                "{bad} of {n} sample points spread across the drive did not verify. \
                 This drive has NOT been erased to the standard claimed above."
            );
        }
        let crypto = self.method.contains("crypto");
        let checked = if crypto {
            "no longer hold their previous contents"
        } else {
            "read back blank"
        };
        let base = format!(
            "{n} sample points spread across the drive, including its first and last \
             sectors, {checked} after the erase."
        );
        if self.had_data {
            format!("{base} Those points held data beforehand and do not now.")
        } else {
            format!(
                "{base} They were already blank beforehand, so this run shows the \
                 command succeeded rather than that data was removed."
            )
        }
    }
}

fn row(k: &str, v: &str) -> String {
    format!("<dt>{}</dt><dd>{}</dd>\n", e(k), e(v))
}

/// A JSON string. Drive fields come off the hardware, so quotes, backslashes
/// and control bytes in a mangled serial must not be able to break the record.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Same job for HTML.
fn e(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const CSS: &str = r#"
:root{--ink:#161a20;--mute:#626b7a;--line:#dfe4ec;--tint:#f5f7fa;
      --ok:#0d6e46;--ok-bg:#e6f4ed;--bad:#a71d18;--bad-bg:#fbecea;--paper:#fff}
*{box-sizing:border-box}
body{margin:0;padding:24px 16px;background:#eceff4;color:var(--ink);
     font:14px/1.55 "Segoe UI",system-ui,sans-serif}
.sheet{max-width:210mm;margin:0 auto;padding:22mm 20mm;background:var(--paper);
       box-shadow:0 1px 3px rgba(20,26,38,.14),0 12px 32px rgba(20,26,38,.10)}
header{display:flex;align-items:flex-start;justify-content:space-between;gap:24px;
       padding-bottom:14px;border-bottom:2px solid var(--ink)}
.eyebrow{margin:0 0 6px;font:600 11px/1 "Segoe UI",sans-serif;letter-spacing:.22em;
         text-transform:uppercase;color:var(--mute)}
h1{margin:0;font:400 30px/1.15 Georgia,"Times New Roman",serif;letter-spacing:-.01em}
.sub{margin:6px 0 0;color:var(--mute);font-size:13px}
.badge{flex:none;text-align:center;padding:10px 16px;border-radius:3px;border:1px solid}
.badge span{display:block;font:700 15px/1 "Segoe UI",sans-serif;letter-spacing:.09em}
.badge small{display:block;margin-top:5px;font-size:10px;letter-spacing:.04em;opacity:.75}
.badge.ok{color:var(--ok);background:var(--ok-bg);border-color:#b7ddc9}
.badge.bad{color:var(--bad);background:var(--bad-bg);border-color:#eebfba}
h2{margin:26px 0 10px;font:600 11px/1 "Segoe UI",sans-serif;letter-spacing:.16em;
   text-transform:uppercase;color:var(--mute)}
.cols{display:grid;grid-template-columns:1fr 1fr;gap:0 40px}
.cols h2{margin-top:22px}
dl{margin:0;display:grid;grid-template-columns:auto 1fr;gap:0}
dt{padding:5px 0;color:var(--mute);font-size:12px;white-space:nowrap;padding-right:16px;
   border-top:1px solid var(--line)}
dd{margin:0;padding:5px 0;text-align:right;border-top:1px solid var(--line);
   font-size:12.5px;overflow-wrap:anywhere}
.lead{margin:0 0 14px;font-size:13.5px}
.map{display:flex;gap:2px;height:34px}
.map i{flex:1;border-radius:1px;background:var(--ok);opacity:.82}
.map i.bad{background:var(--bad);opacity:1}
.scale{display:flex;justify-content:space-between;margin:5px 0 18px;
       font-size:10px;letter-spacing:.05em;text-transform:uppercase;color:var(--mute)}
table{width:100%;border-collapse:collapse;font-size:10.5px}
th{text-align:left;padding:5px 6px;border-bottom:1px solid var(--ink);
   font-weight:600;color:var(--mute);letter-spacing:.04em}
td{padding:4px 6px;border-bottom:1px solid var(--line)}
tbody tr:nth-child(even){background:var(--tint)}
.hex{font:10.5px/1.4 Consolas,"Cascadia Mono",monospace;color:#3b4453;letter-spacing:.02em}
td.pass{color:var(--ok);font-weight:600}
td.fail{color:var(--bad);font-weight:700}
.limits{margin-top:26px;padding:14px 18px;background:var(--tint);
        border-left:3px solid var(--mute)}
.limits h2{margin-top:0}
.limits ul{margin:0;padding-left:18px}
.limits li{margin:5px 0;font-size:12px;color:#3b4453}
footer{display:flex;gap:40px;margin-top:34px}
.sign{flex:1}
.sign span{display:block;height:34px;border-bottom:1px solid var(--ink)}
.sign small{display:block;margin-top:5px;font-size:10px;letter-spacing:.05em;
            text-transform:uppercase;color:var(--mute)}
.fine{margin:22px 0 0;padding-top:12px;border-top:1px solid var(--line);
      font-size:10.5px;line-height:1.5;color:var(--mute)}
@media print{
  body{background:#fff;padding:0;print-color-adjust:exact;-webkit-print-color-adjust:exact}
  .sheet{box-shadow:none;padding:0;max-width:none}
  @page{margin:16mm}
  section,footer{break-inside:avoid}
  thead{display:table-header-group}
}
@media (max-width:640px){
  .sheet{padding:16px}
  .cols{grid-template-columns:1fr}
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(passed: bool) -> Cert {
        Cert {
            when: "2026-08-09T21:43:12Z".into(),
            host: "BENCH".into(),
            operator: "BENCH\\operator".into(),
            tool: "bulkhead 0.1.0".into(),
            disk: 5,
            model: "Generic \"Reader\" <USB>".into(),
            serial: "AB\\CD\t01".into(),
            firmware: "1.00".into(),
            bus: "USB".into(),
            size: 7_948_206_080,
            method: "overwrite".into(),
            seconds: 754,
            had_data: true,
            points: vec![
                Point {
                    at: 0,
                    before: "eb52904e54465320".into(),
                    after: "0".repeat(16),
                    ok: true,
                },
                Point {
                    at: 4096,
                    before: "ff".repeat(8),
                    after: "ff".repeat(8),
                    ok: passed,
                },
            ],
            passed,
            caveats: vec!["an overwrite cannot reach remapped blocks".into()],
        }
    }

    #[test]
    fn drive_strings_cannot_break_the_json() {
        // Model and serial come off the hardware. A quote or a tab in either
        // must not be able to produce a record that will not parse.
        let j = sample(true).json();
        assert!(j.contains(r#""model": "Generic \"Reader\" <USB>""#), "{j}");
        assert!(j.contains(r#""serial": "AB\\CD\t01""#), "{j}");
        assert_eq!(j.matches('{').count(), j.matches('}').count());
        // Trailing commas are the other way this stops being JSON.
        assert!(!j.contains(",\n  ]") && !j.contains(",\n}"), "{j}");
    }

    #[test]
    fn a_failed_erase_still_produces_a_certificate_that_says_so() {
        // The failure mode worth guarding: a document that only ever prints
        // VERIFIED is worse than no document.
        let c = sample(false);
        assert_eq!(c.verdict(), "FAILED");
        let h = c.html();
        assert!(h.contains("badge bad"));
        assert!(h.contains("has NOT been erased"), "{}", c.summary());
        assert!(c.json().contains("\"verified\": false"));

        let good = sample(true);
        assert_eq!(good.verdict(), "VERIFIED");
        assert!(good.html().contains("badge ok"));
        assert!(!good.html().contains("has NOT been erased"));
    }

    #[test]
    fn html_escapes_what_the_drive_reported() {
        let h = sample(true).html();
        assert!(h.contains("Generic &quot;Reader&quot; &lt;USB&gt;"), "{h}");
        assert!(!h.contains("<USB>"));
    }

    #[test]
    fn the_standard_tracks_what_the_method_can_reach() {
        // Clear vs Purge is not cosmetic: it is the remapped-block difference,
        // and claiming Purge for a host overwrite is the lie worth avoiding.
        assert_eq!(standard("overwrite"), "Clear");
        assert_eq!(standard("ata-sanitize-crypto"), "Purge");
        assert_eq!(standard("ata-sanitize-block"), "Purge");
        assert_eq!(standard("nvme-sanitize-block"), "Purge");
        assert_eq!(standard("nvme-format"), "Clear");
    }

    #[test]
    fn an_already_blank_drive_is_not_reported_as_data_removed() {
        let mut c = sample(true);
        c.had_data = false;
        assert!(c.summary().contains("already blank beforehand"));
        c.had_data = true;
        assert!(c.summary().contains("held data beforehand"));
    }
}
