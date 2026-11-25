use std::{
    io::{self, BufRead, Write},
    time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_EXPIRY_DAYS: u64 = 7;
const MAX_STORED_VALUE: u64 = i64::MAX as u64;

pub(super) struct ShareOptions {
    pub expiry_days: Option<u64>,
    pub max_downloads: Option<u64>,
    pub password: Option<String>,
}

pub(super) fn prompt(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Option<ShareOptions>> {
    // The server stores the absolute expiration in a signed 64-bit integer.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_expiry_days = MAX_STORED_VALUE.saturating_sub(now) / 86_400;
    let Some(expiry_days) = prompt_limit(
        input,
        output,
        "Expiry in days (or forever) [7]: ",
        "forever",
        Some(DEFAULT_EXPIRY_DAYS),
        max_expiry_days,
    )?
    else {
        return Ok(None);
    };
    let Some(max_downloads) = prompt_limit(
        input,
        output,
        "Max downloads [unlimited]: ",
        "unlimited",
        None,
        MAX_STORED_VALUE,
    )?
    else {
        return Ok(None);
    };
    let password = loop {
        let Some(value) = ask(input, output, "Share password [none]: ")? else {
            return Ok(None);
        };
        if value.is_empty() {
            break None;
        }
        if value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            break Some(value);
        }
        writeln!(output, "Use 1 to 64 ASCII letters or digits.")?;
    };

    let expiry = expiry_days.map_or_else(|| "forever".to_owned(), |days| format!("{days} days"));
    let downloads = max_downloads.map_or_else(|| "unlimited".to_owned(), |count| count.to_string());
    let protection = if password.is_some() {
        "enabled"
    } else {
        "none"
    };
    writeln!(
        output,
        "Expiry: {expiry}, max downloads: {downloads}, share password: {protection}"
    )?;
    loop {
        let Some(answer) = ask(input, output, "Upload [y/N]? ")? else {
            return Ok(None);
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => {
                return Ok(Some(ShareOptions {
                    expiry_days,
                    max_downloads,
                    password,
                }));
            }
            "" | "n" | "no" => return Ok(None),
            _ => writeln!(output, "Enter y or n.")?,
        }
    }
}

// The outer option represents EOF. The inner option distinguishes a limit from an unlimited value.
fn prompt_limit(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    unlimited: &str,
    default: Option<u64>,
    maximum: u64,
) -> io::Result<Option<Option<u64>>> {
    loop {
        let Some(value) = ask(input, output, label)? else {
            return Ok(None);
        };
        let value = value.trim();
        if value.is_empty() {
            return Ok(Some(default));
        }
        if value.eq_ignore_ascii_case(unlimited) {
            return Ok(Some(None));
        }
        if let Ok(number) = value.parse::<u64>()
            && (1..=maximum).contains(&number)
        {
            return Ok(Some(Some(number)));
        }
        writeln!(
            output,
            "Enter a positive whole number up to {maximum}, or {unlimited}."
        )?;
    }
}

fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
) -> io::Result<Option<String>> {
    write!(output, "{label}")?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    line.truncate(line.trim_end_matches(['\r', '\n']).len());
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_web_defaults() {
        let options = prompt(&mut &b"\n\n\ny\n"[..], &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(options.expiry_days, Some(7));
        assert_eq!(options.max_downloads, None);
        assert_eq!(options.password, None);
    }

    #[test]
    fn retries_invalid_inputs_and_accepts_explicit_options() {
        let mut output = Vec::new();
        let options = prompt(
            &mut &b"0\nforever\n1.5\n3\npw with spaces\nP1GEON\nyes\n"[..],
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.expiry_days, None);
        assert_eq!(options.max_downloads, Some(3));
        assert_eq!(options.password.as_deref(), Some("P1GEON"));
        let written = String::from_utf8(output).unwrap();
        assert!(written.contains("Enter a positive whole number"));
        assert!(written.contains("Use 1 to 64 ASCII letters or digits."));
        assert!(!written.contains("P1GEON"));
    }

    #[test]
    fn requires_affirmative_confirmation() {
        for input in ["", "\n\n\n", "\n\n\n\n", "\n\n\nno\n"] {
            assert!(
                prompt(&mut input.as_bytes(), &mut Vec::new())
                    .unwrap()
                    .is_none()
            );
        }
    }
}
