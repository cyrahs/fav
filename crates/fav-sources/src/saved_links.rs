//! The posts saved from links, by canonical URL, the same for Telegram and WeChat: a 公众号
//! article, Tieba or NGA thread, or Zhihu answer, article or pin is saved once, whichever
//! source it comes through and however often it is sent.
//!
//! No Python origin: there Telegram kept one row per message and never looked at the URL,
//! and WeChat looked only at its own `wechat_article` rows (DIVERGENCES.md D188).
//!
//! The key is the URL [`crate::post_links::link_for_url`] builds and
//! [`crate::wechat_article::canonical_url`] keeps: a Zhihu answer is
//! `https://www.zhihu.com/answer/<id>` whether it was shared under its question or not, and
//! share parameters are gone. A source looks the link up with [`find`] before it reads the
//! post, skips one that is already saved and tells the user with [`already_saved`], and
//! after a save adds the link with [`record`]. The posts saved before the table existed
//! are carried into it once, at startup, by [`carry_over`].

use chrono::{DateTime, Utc};
use fav_db::Db;
use fav_runtime::notifications::NewNotification;
use sqlx::Row;

/// The notification kind of a link that was not saved again.
pub const KIND_ALREADY_SAVED: &str = "already_saved";

/// A post saved earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedLink {
    pub url: String,
    /// The job key of the source that saved it (`telegram`, `wechat`).
    pub source: String,
    /// The message it came in, as that source names one (`fav #143`, `message 123456`).
    pub item: String,
    pub title: String,
    /// Where it was saved: the first file, or the article's folder. Empty for some posts
    /// saved before the table existed.
    pub saved_path: String,
    pub saved_at: DateTime<Utc>,
}

impl SavedLink {
    /// The source's name, as the notifications head it.
    fn source_name(&self) -> &str {
        match self.source.as_str() {
            "telegram" => "Telegram",
            "wechat" => "WeChat",
            "zhihu" => "Zhihu",
            other => other,
        }
    }
}

/// A post being saved now.
#[derive(Debug, Clone, Copy)]
pub struct NewSavedLink<'a> {
    pub url: &'a str,
    pub source: &'a str,
    pub item: &'a str,
    pub title: &'a str,
    pub saved_path: &'a str,
}

/// The saved post at `url`, a canonical URL, if there is one.
pub async fn find(db: &Db, url: &str) -> Result<Option<SavedLink>, sqlx::Error> {
    let row = sqlx::query("SELECT url, source, item, title, saved_path, saved_at FROM saved_post_link WHERE url = $1")
        .bind(url)
        .fetch_optional(db.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(SavedLink {
        url: row.try_get("url")?,
        source: row.try_get("source")?,
        item: row.try_get("item")?,
        title: row.try_get("title")?,
        saved_path: row.try_get("saved_path")?,
        saved_at: row.try_get("saved_at")?,
    }))
}

/// Add a saved post. The first save of a URL stays; a second one, which only a race
/// between the two sources can make, changes nothing.
pub async fn record(db: &Db, link: NewSavedLink<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO saved_post_link (url, source, item, title, saved_path)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (url) DO NOTHING",
    )
    .bind(link.url)
    .bind(link.source)
    .bind(link.item)
    .bind(link.title)
    .bind(link.saved_path)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// The posts saved from links before the table existed, while it is still empty. WeChat's
/// are its `wechat_article` rows. Telegram kept no URL, so its saves are read back from
/// their download notifications, the only rows that name the post. The earliest save of a
/// URL is kept. Once the table has a row this adds nothing, so a row deleted on purpose
/// (to let a post be saved again) does not come back at the next start. The rows added.
pub async fn carry_over(db: &Db) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        r#"INSERT INTO saved_post_link (url, source, item, title, saved_path, saved_at)
        SELECT DISTINCT ON (url) url, source, item, title, saved_path, saved_at
        FROM (
            SELECT
                url,
                'wechat' AS source,
                'message ' || msg_id AS item,
                title,
                local_path AS saved_path,
                COALESCE(created_at AT TIME ZONE 'UTC', CURRENT_TIMESTAMP) AS saved_at
            FROM wechat_article
            WHERE url <> ''
            UNION ALL
            SELECT
                link_url,
                'telegram',
                COALESCE(
                    substring(payload FROM '"channel_name":\s*"([^"]*)"') || ' #' || substring(payload FROM '"message_id":\s*([0-9]+)'),
                    ''
                ),
                title,
                '',
                created_at
            FROM notifications
            WHERE source = 'telegram' AND kind = 'download_completed' AND link_url <> ''
        ) AS saved
        WHERE NOT EXISTS (SELECT 1 FROM saved_post_link)
        ORDER BY url, saved_at
        ON CONFLICT (url) DO NOTHING"#,
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected())
}

/// Why a message's link is not saved again, as its queue row or log records it.
pub fn reason(saved: &SavedLink) -> String {
    let from = if saved.item.is_empty() {
        saved.source_name().to_owned()
    } else {
        format!("{} {}", saved.source_name(), saved.item)
    };
    format!("Already saved from {from} on {}", saved.saved_at.format("%Y-%m-%d"))
}

/// The message telling the user that a link they sent was saved before: under the title
/// it came with now, linking to the post, naming where the earlier copy is.
pub fn already_saved(source: &str, header: &str, title: &str, saved: &SavedLink) -> NewNotification {
    let body = if saved.saved_path.is_empty() {
        format!("{}, not saved again", reason(saved))
    } else {
        format!("{}, not saved again: {}", reason(saved), saved.saved_path)
    };
    let title = if title.is_empty() { &saved.title } else { title };
    NewNotification {
        kind: KIND_ALREADY_SAVED.to_owned(),
        source: source.to_owned(),
        header: header.to_owned(),
        title: title.to_owned(),
        link_url: saved.url.clone(),
        body,
        ..NewNotification::default()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use fav_db::testdb::TestDatabase;

    use super::*;

    async fn database() -> TestDatabase {
        let database = TestDatabase::create().await;
        database.db().ensure_schema().await.expect("schema applies");
        database
    }

    const ANSWER: &str = "https://www.zhihu.com/answer/2100311203096573240";

    #[tokio::test]
    async fn the_first_save_of_a_url_stays() {
        let database = database().await;
        let db = database.db();
        assert_eq!(find(db, ANSWER).await.expect("find"), None);
        let first = NewSavedLink {
            url: ANSWER,
            source: "telegram",
            item: "fav #123",
            title: "高跟鞋配白丝真的很难看吗？",
            saved_path: "collection/image/a [123].jpg",
        };
        record(db, first).await.expect("record");
        record(
            db,
            NewSavedLink {
                source: "wechat",
                item: "message 9",
                ..first
            },
        )
        .await
        .expect("a second save is ignored");
        let saved = find(db, ANSWER).await.expect("find").expect("saved");
        assert_eq!(
            (saved.source.as_str(), saved.item.as_str(), saved.saved_path.as_str()),
            ("telegram", "fav #123", "collection/image/a [123].jpg")
        );
        assert_eq!(find(db, "https://www.zhihu.com/answer/1").await.expect("find"), None);
        database.drop_database().await;
    }

    async fn execute(db: &Db, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(db.pool())
            .await
            .expect("statement");
    }

    async fn rows(db: &Db) -> Vec<(String, String, String, String, String)> {
        sqlx::query("SELECT url, source, item, title, saved_path FROM saved_post_link ORDER BY url")
            .fetch_all(db.pool())
            .await
            .expect("rows")
            .iter()
            .map(|row| {
                (
                    row.get("url"),
                    row.get("source"),
                    row.get("item"),
                    row.get("title"),
                    row.get("saved_path"),
                )
            })
            .collect()
    }

    /// The posts saved before the table existed are carried into it, once.
    #[tokio::test]
    async fn posts_saved_before_the_table_are_carried_into_it_once() {
        let database = database().await;
        let db = database.db();
        execute(
            db,
            "INSERT INTO wechat_article (msg_id, create_time, url, title, local_path, created_at) VALUES
                (7, 1, 'https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b', 'A', '2026-11/a [7]', '2026-11-01 00:00:00'),
                (8, 1, '', 'no url', '', '2026-11-01 00:00:00');
            INSERT INTO notifications (kind, source, title, link_url, payload, created_at) VALUES
                ('download_completed', 'telegram', 'Q', 'https://www.zhihu.com/answer/2100311203096573240',
                 '{\"account_name\":\"cyrah\",\"channel_name\":\"fav\",\"message_id\":143}', '2026-11-09 20:58:51+00'),
                ('download_completed', 'telegram', 'Q', 'https://www.zhihu.com/answer/2100311203096573240',
                 '{\"account_name\":\"cyrah\",\"channel_name\":\"fav\",\"message_id\":123}', '2026-11-06 18:44:27+00'),
                ('download_completed', 'telegram', 'clip', '', '{\"message_id\":5}', '2026-11-06 00:00:00+00'),
                ('download_failed', 'telegram', 'gone', 'https://www.zhihu.com/answer/1', '{}', '2026-11-06 00:00:00+00'),
                ('download_completed', 'wechat', 'A', 'https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b', '{}', '2026-11-01 00:00:00+00');",
        )
        .await;

        assert_eq!(carry_over(db).await.expect("carried over"), 2);

        let carried = vec![
            (
                "https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b".to_owned(),
                "wechat".to_owned(),
                "message 7".to_owned(),
                "A".to_owned(),
                "2026-11/a [7]".to_owned(),
            ),
            (
                ANSWER.to_owned(),
                "telegram".to_owned(),
                "fav #123".to_owned(),
                "Q".to_owned(),
                String::new(),
            ),
        ];
        assert_eq!(rows(db).await, carried);
        let saved = find(db, ANSWER).await.expect("find").expect("carried");
        assert_eq!(
            saved.saved_at,
            Utc.with_ymd_and_hms(2026, 11, 6, 18, 44, 27).single().expect("time")
        );

        // Once the table has rows, a later start adds none, and a row deleted on purpose stays deleted.
        execute(
            db,
            "INSERT INTO wechat_article (msg_id, create_time, url) VALUES (9, 1, 'https://tieba.baidu.com/p/9');
            DELETE FROM saved_post_link WHERE source = 'wechat';",
        )
        .await;
        assert_eq!(carry_over(db).await.expect("nothing to carry"), 0);
        assert_eq!(rows(db).await, carried[1..]);
        database.drop_database().await;
    }

    #[test]
    fn the_notification_names_the_earlier_copy() {
        let saved = SavedLink {
            url: ANSWER.to_owned(),
            source: "telegram".to_owned(),
            item: "fav #123".to_owned(),
            title: "old title".to_owned(),
            saved_path: "collection/image/a [123].jpg".to_owned(),
            saved_at: Utc.with_ymd_and_hms(2026, 11, 6, 18, 44, 27).single().expect("time"),
        };
        let sent = already_saved("wechat", "WeChat", "new title", &saved);
        assert_eq!(sent.kind, "already_saved");
        assert_eq!((sent.source.as_str(), sent.header.as_str()), ("wechat", "WeChat"));
        assert_eq!(sent.title, "new title");
        assert_eq!(sent.link_url, ANSWER);
        assert_eq!(
            sent.body,
            "Already saved from Telegram fav #123 on 2026-11-06, not saved again: collection/image/a [123].jpg"
        );
        let untitled = already_saved(
            "telegram",
            "Telegram",
            "",
            &SavedLink {
                item: String::new(),
                saved_path: String::new(),
                ..saved
            },
        );
        assert_eq!(untitled.title, "old title");
        assert_eq!(untitled.body, "Already saved from Telegram on 2026-11-06, not saved again");
    }
}
