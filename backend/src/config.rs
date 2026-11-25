use std::{collections::HashMap, path::PathBuf};

use clap::Args;

#[derive(Args)]
pub struct ServeArguments {
    #[arg(
        long = "user",
        value_name = "USERNAME:PASSWORD",
        help = "Login account; repeat to add users"
    )]
    users: Vec<String>,

    #[arg(long, default_value_t = 3000, help = "HTTP port")]
    pub port: u16,

    #[arg(long, default_value = "/", help = "Public deployment path")]
    base_path: String,

    #[arg(
        long,
        value_name = "SIZE",
        value_parser = parse_file_size,
        help = "Maximum upload file size, e.g. 500MB or 2GB. Omit for unlimited."
    )]
    pub max_file_size: Option<usize>,

    #[arg(
        long,
        default_value = "./pigeon-data",
        help = "Persistent files and SQLite database"
    )]
    pub data: PathBuf,
}

impl ServeArguments {
    pub fn accounts(&self) -> Result<HashMap<String, String>, &'static str> {
        let mut accounts = HashMap::new();
        for account in &self.users {
            let (name, password) = parse_credentials(account)?;
            if accounts
                .insert(name.to_owned(), password.to_owned())
                .is_some()
            {
                return Err("Duplicate username in --user");
            }
        }
        Ok(accounts)
    }

    pub fn base_path(&self) -> Result<BasePath, &'static str> {
        self.base_path.parse()
    }
}

pub(crate) fn parse_credentials(value: &str) -> Result<(&str, &str), &'static str> {
    // Split on the first colon so the password can contain additional colons.
    let (name, password) = value
        .split_once(':')
        .ok_or("--user must be USERNAME:PASSWORD")?;
    if !valid_username(name) || password.is_empty() || password.len() > 1024 {
        return Err("Invalid username or password in --user");
    }
    Ok((name, password))
}

fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.chars().any(char::is_control)
        && name.trim() == name
}

fn parse_file_size(value: &str) -> Result<usize, &'static str> {
    const INVALID_SIZE: &str =
        "expected a positive size such as 500MB or 2GB within the supported range";
    let value = value.trim();
    let unit_start = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (amount, unit) = value.split_at(unit_start);
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(INVALID_SIZE),
    };
    amount
        .parse::<u64>()
        .ok()
        .and_then(|amount| amount.checked_mul(multiplier))
        // SQLite stores file sizes as signed 64-bit integers.
        .filter(|&bytes| (1..=i64::MAX as u64).contains(&bytes))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(INVALID_SIZE)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasePath(String);

impl std::str::FromStr for BasePath {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(rest) = value.strip_prefix('/') else {
            return Err("--base-path must start with /");
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if value == "/" {
            return Ok(Self(String::new()));
        }
        if rest.is_empty()
            || rest.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
            })
        {
            return Err("--base-path must contain ordinary URL path segments");
        }
        Ok(Self(format!("/{rest}")))
    }
}

impl BasePath {
    pub fn home(&self) -> &str {
        if self.0.is_empty() { "/" } else { &self.0 }
    }

    pub fn path(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arguments, Command};
    use clap::Parser;

    fn serve_args<const N: usize>(options: [&str; N]) -> Result<ServeArguments, clap::Error> {
        let args = Arguments::try_parse_from(["pigeon", "serve"].into_iter().chain(options))?;
        let Command::Serve(args) = args.command else {
            unreachable!();
        };
        Ok(args)
    }

    #[test]
    fn parses_max_file_size_in_bytes_and_supported_units() {
        let args = serve_args([]).unwrap();
        assert_eq!(args.max_file_size, None);
        for (value, bytes) in [
            ("128", 128),
            ("32B", 32),
            ("500MB", 500_000_000),
            ("2GB", 2_000_000_000),
            ("5 KiB", 5 * 1024),
            ("2mib", 2 * 1024 * 1024),
        ] {
            let args = serve_args(["--max-file-size", value]).unwrap();
            assert_eq!(args.max_file_size, Some(bytes));
        }
        for value in [
            "0",
            "-1",
            "KiB",
            "1.5GiB",
            "1XB",
            "9223372036854775808",
            "18446744073709551615MiB",
        ] {
            assert!(serve_args(["--max-file-size", value]).is_err(), "{value}");
        }
    }

    #[test]
    fn normalizes_configuration_into_exact_routes() {
        assert_eq!(
            "/".parse::<BasePath>().unwrap().path("/sharing"),
            "/sharing"
        );
        assert_eq!(
            "/pigeon/sub/".parse::<BasePath>().unwrap().home(),
            "/pigeon/sub"
        );
        for path in ["pigeon", "//", "/a//b", "/a//", "/../a", "/a?b", "/a%2fb"] {
            assert!(path.parse::<BasePath>().is_err(), "{path}");
        }
    }

    #[test]
    fn validates_credentials_and_splits_on_the_first_colon() {
        assert_eq!(parse_credentials("alice:p:a"), Ok(("alice", "p:a")));
        for value in ["alice", "alice:", ":password", " alice:password"] {
            assert!(parse_credentials(value).is_err(), "{value}");
        }
    }

    #[test]
    fn accepts_multiple_unique_accounts() {
        let args = serve_args(["--user", "alice:p:a", "--user", "bob:p"]).unwrap();
        assert_eq!(args.accounts().unwrap()["alice"], "p:a");
        assert_eq!(args.accounts().unwrap()["bob"], "p");
        let args = serve_args(["--user", "alice:p", "--user", "alice:q"]).unwrap();
        assert!(args.accounts().is_err());
    }
}
