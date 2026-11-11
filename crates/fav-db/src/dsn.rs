//! `POSTGRES_DSN` in either form libpq (and so psycopg) accepts: a URL
//! (`postgresql://user@host/db`) or key/value conninfo (`host=db user=fav dbname=fav`).

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgSslMode};

use crate::DbError;

/// Parse a DSN into connect options. Unset parts fall back to the `PG*` environment
/// variables and libpq's defaults, as with psycopg.
pub fn parse_dsn(dsn: &str) -> Result<PgConnectOptions, DbError> {
    let trimmed = dsn.trim();
    if trimmed.starts_with("postgresql://") || trimmed.starts_with("postgres://") {
        return Ok(PgConnectOptions::from_str(trimmed)?);
    }
    let mut options = PgConnectOptions::new();
    for (key, value) in conninfo_pairs(trimmed)? {
        options = match key.as_str() {
            "host" | "hostaddr" => options.host(&value),
            "port" => options.port(value.parse().map_err(|_| invalid(format!("invalid port number: \"{value}\"")))?),
            "user" => options.username(&value),
            "password" => options.password(&value),
            "dbname" => options.database(&value),
            "sslmode" => options.ssl_mode(PgSslMode::from_str(&value).map_err(|_| invalid(format!("invalid sslmode value: \"{value}\"")))?),
            "sslrootcert" => options.ssl_root_cert(&value),
            "sslcert" => options.ssl_client_cert(&value),
            "sslkey" => options.ssl_client_key(&value),
            "application_name" => options.application_name(&value),
            "options" => options.options(parse_startup_options(&value)),
            // The pool's own acquire timeout bounds connecting; libpq's is not needed.
            "connect_timeout" => options,
            other => return Err(invalid(format!("invalid connection option \"{other}\""))),
        };
    }
    Ok(options)
}

fn invalid(message: String) -> DbError {
    DbError::Invalid(message)
}

/// `-c name=value` pairs of libpq's `options` parameter.
fn parse_startup_options(value: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = value.split_whitespace().collect();
    let mut pairs = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let setting = match words[index] {
            "-c" => {
                index += 1;
                words.get(index).copied()
            }
            word => word.strip_prefix("-c").or_else(|| word.strip_prefix("--")),
        };
        if let Some((name, setting_value)) = setting.and_then(|setting| setting.split_once('=')) {
            pairs.push((name.replace('-', "_"), setting_value.to_owned()));
        }
        index += 1;
    }
    pairs
}

/// libpq's conninfo grammar: `key = value` pairs separated by whitespace; a value is either
/// single-quoted or runs to the next whitespace, and `\` escapes the next character.
fn conninfo_pairs(text: &str) -> Result<Vec<(String, String)>, DbError> {
    let mut pairs = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(pairs);
        }
        let mut key = String::new();
        while let Some(ch) = chars.next_if(|ch| *ch != '=' && !ch.is_whitespace()) {
            key.push(ch);
        }
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        if chars.next() != Some('=') {
            return Err(invalid(format!("missing \"=\" after \"{key}\" in connection info string")));
        }
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\'') => break,
                    Some('\\') => value.extend(chars.next()),
                    Some(ch) => value.push(ch),
                    None => return Err(invalid("unterminated quoted string in connection info string".to_owned())),
                }
            }
        } else {
            while let Some(ch) = chars.next_if(|ch| !ch.is_whitespace()) {
                if ch == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(ch);
                }
            }
        }
        pairs.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_conninfo_give_the_same_options() {
        let url = parse_dsn("postgresql://fav:s3cret@db.local:5433/favdb?sslmode=disable").expect("url");
        let conninfo = parse_dsn("host=db.local port = 5433 user=fav password='s3cret' dbname=favdb sslmode=disable").expect("conninfo");
        for options in [&url, &conninfo] {
            assert_eq!(options.get_host(), "db.local");
            assert_eq!(options.get_port(), 5433);
            assert_eq!(options.get_username(), "fav");
            assert_eq!(options.get_database(), Some("favdb"));
            assert!(matches!(options.get_ssl_mode(), PgSslMode::Disable));
        }
    }

    #[test]
    fn conninfo_quoting_and_escapes() {
        let pairs = conninfo_pairs(r"password='it\'s a pass' application_name=a\ b options='-c search_path=fav'").expect("valid");
        assert_eq!(
            pairs,
            [
                ("password".to_owned(), "it's a pass".to_owned()),
                ("application_name".to_owned(), "a b".to_owned()),
                ("options".to_owned(), "-c search_path=fav".to_owned()),
            ]
        );
        assert_eq!(
            parse_startup_options("-c search_path=fav --statement-timeout=5s"),
            [
                ("search_path".to_owned(), "fav".to_owned()),
                ("statement_timeout".to_owned(), "5s".to_owned()),
            ]
        );
    }

    #[test]
    fn bad_conninfo_is_refused_like_libpq() {
        let message = |dsn: &str| parse_dsn(dsn).expect_err("refused").to_string();
        assert_eq!(message("host=db bogus=1"), "invalid connection option \"bogus\"");
        assert_eq!(message("host"), "missing \"=\" after \"host\" in connection info string");
        assert_eq!(message("password='open"), "unterminated quoted string in connection info string");
        assert_eq!(message("port=http"), "invalid port number: \"http\"");
    }

    #[tokio::test]
    async fn a_conninfo_dsn_connects() {
        let database = crate::testdb::TestDatabase::create().await;
        let url = std::env::var(crate::testdb::URL_VARIABLE).expect("set by TestDatabase::create");
        let base = PgConnectOptions::from_str(&url).expect("url");
        let dsn = format!(
            "host={} port={} user={} dbname={}",
            base.get_host(),
            base.get_port(),
            base.get_username(),
            database.name()
        );
        let db = crate::Db::connect_lazy(&dsn).expect("conninfo");
        let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(db.pool()).await.expect("query");
        assert_eq!(one, 1);
        db.close().await;
        database.drop_database().await;
    }
}
