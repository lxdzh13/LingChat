use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// 四张热词表收敛为 `hotword` 单表（按 scope 分区）+ `hotword_category`。
///
/// 迁移前按新唯一键去重：`GROUP BY` + `MAX(weight)`，否则仅空白不同的旧行会撞新索引。
/// 分类表重建以补非空/长度 CHECK，保留原 id 让词库行不悬空。
/// 脚本显式开事务：SQLite 的 DDL 逐条提交，中途失败会留下半成品。
const UP_SQL: &str = r#"BEGIN;

-- 分类表重建：补非空/长度 CHECK，保留原 id（library 行仍按 id 引用）
CREATE TABLE hotword_category_new (
    id   INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    name TEXT    NOT NULL,
    UNIQUE (name),
    CHECK (length(trim(name, char(9) || char(10) || char(13) || ' ')) BETWEEN 1 AND 30)
);
INSERT INTO hotword_category_new (id, name) SELECT id, name FROM hotword_category;
DROP TABLE hotword_category;
ALTER TABLE hotword_category_new RENAME TO hotword_category;

CREATE TABLE hotword (
    id          INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    scope       TEXT    NOT NULL,
    role_id     INTEGER,
    category_id INTEGER,
    text        TEXT    NOT NULL,
    weight      INTEGER NOT NULL DEFAULT 4,
    CHECK (scope IN ('role', 'global', 'library')),
    CHECK (
        (scope = 'role'    AND role_id IS NOT NULL AND category_id IS NULL)
        OR (scope = 'global'  AND role_id IS NULL     AND category_id IS NULL)
        OR (scope = 'library' AND role_id IS NULL
            AND (category_id IS NULL OR category_id > 0))
    ),
    CHECK (weight IN (1, 2, 3, 4, 5, 50)),
    CHECK (length(trim(text, char(9) || char(10) || char(13) || ' ')) > 0)
);

INSERT INTO hotword (scope, role_id, category_id, text, weight)
    SELECT 'role', role_id, NULL, text, MAX(weight)
    FROM (
        SELECT id, role_id,
               trim(text, char(9) || char(10) || char(13) || ' ') AS text,
               CASE WHEN weight BETWEEN 1 AND 5 THEN weight
                    WHEN weight >= 50 THEN 50
                    ELSE 5 END AS weight
        FROM role_hotword
        WHERE length(trim(text, char(9) || char(10) || char(13) || ' ')) > 0
    )
    GROUP BY role_id, text
    ORDER BY MIN(id);

INSERT INTO hotword (scope, role_id, category_id, text, weight)
    SELECT 'global', NULL, NULL, text, MAX(weight)
    FROM (
        SELECT id,
               trim(text, char(9) || char(10) || char(13) || ' ') AS text,
               CASE WHEN weight BETWEEN 1 AND 5 THEN weight
                    WHEN weight >= 50 THEN 50
                    ELSE 5 END AS weight
        FROM global_hotword
        WHERE length(trim(text, char(9) || char(10) || char(13) || ' ')) > 0
    )
    GROUP BY text
    ORDER BY MIN(id);

INSERT INTO hotword (scope, role_id, category_id, text, weight)
    SELECT 'library', NULL, category_id, text, MAX(weight)
    FROM (
        SELECT id, category_id,
               trim(text, char(9) || char(10) || char(13) || ' ') AS text,
               CASE WHEN weight BETWEEN 1 AND 5 THEN weight
                    WHEN weight >= 50 THEN 50
                    ELSE 5 END AS weight
        FROM library_hotword
        WHERE length(trim(text, char(9) || char(10) || char(13) || ' ')) > 0
    )
    GROUP BY category_id, text
    ORDER BY MIN(id);

DROP TABLE role_hotword;
DROP TABLE global_hotword;
DROP TABLE library_hotword;

CREATE UNIQUE INDEX ux_hotword_role_text ON hotword (role_id, text) WHERE scope = 'role';
CREATE UNIQUE INDEX ux_hotword_global_text ON hotword (text) WHERE scope = 'global';
CREATE UNIQUE INDEX ux_hotword_library_category_text ON hotword (category_id, text)
    WHERE scope = 'library' AND category_id IS NOT NULL;
CREATE UNIQUE INDEX ux_hotword_library_uncategorized_text ON hotword (text)
    WHERE scope = 'library' AND category_id IS NULL;
CREATE INDEX ix_hotword_scope_id ON hotword (scope, id);

COMMIT;
"#;

/// 按 scope 拆回三张旧表并重建旧约束；`hotword_category` 不动。
const DOWN_SQL: &str = r#"BEGIN;
CREATE TABLE role_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    role_id INTEGER NOT NULL,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4 CHECK (weight BETWEEN 1 AND 50),
    UNIQUE (role_id, text)
);
INSERT INTO role_hotword (role_id, text, weight)
    SELECT role_id, text, weight FROM hotword WHERE scope = 'role' ORDER BY id;
CREATE TABLE global_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4 CHECK (weight BETWEEN 1 AND 50),
    UNIQUE (text)
);
INSERT INTO global_hotword (text, weight)
    SELECT text, weight FROM hotword WHERE scope = 'global' ORDER BY id;
CREATE TABLE library_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    category_id INTEGER,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4 CHECK (weight BETWEEN 1 AND 50)
);
INSERT INTO library_hotword (category_id, text, weight)
    SELECT category_id, text, weight FROM hotword WHERE scope = 'library' ORDER BY id;
CREATE UNIQUE INDEX ux_library_hotword_category_text
    ON library_hotword (category_id, text) WHERE category_id IS NOT NULL;
CREATE UNIQUE INDEX ux_library_hotword_uncategorized_text
    ON library_hotword (text) WHERE category_id IS NULL;
DROP TABLE hotword;
COMMIT;
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP_SQL).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DOWN_SQL)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{
        ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement, TryGetable,
    };

    use super::{DOWN_SQL, UP_SQL};

    const OLD_SCHEMA: &str = "\
CREATE TABLE role_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    role_id INTEGER NOT NULL,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4,
    UNIQUE (role_id, text)
);
CREATE TABLE global_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4,
    UNIQUE (text)
);
CREATE TABLE hotword_category (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    UNIQUE (name)
);
CREATE TABLE library_hotword (
    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    category_id INTEGER,
    text TEXT NOT NULL,
    weight INTEGER NOT NULL DEFAULT 4
);
CREATE UNIQUE INDEX ux_library_hotword_category_text
    ON library_hotword (category_id, text) WHERE category_id IS NOT NULL;
CREATE UNIQUE INDEX ux_library_hotword_uncategorized_text
    ON library_hotword (text) WHERE category_id IS NULL;
INSERT INTO hotword_category (id, name) VALUES (3, '人物');
INSERT INTO role_hotword (role_id, text, weight) VALUES (1, '张三', 4), (1, '张三 ', 5), (2, '张三', 3);
INSERT INTO global_hotword (text, weight) VALUES ('通用', 4), ('轻越界', 7), ('重越界', 99);
INSERT INTO library_hotword (category_id, text, weight) VALUES (3, '钦灵', 4), (NULL, '通用', 4);
";

    async fn scalar<T: TryGetable>(db: &DatabaseConnection, sql: &str) -> T {
        db.query_one(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await
            .expect("查询失败")
            .expect("查询无结果")
            .try_get_by_index(0)
            .expect("读取标量失败")
    }

    /// 直接执行 up/down 脚本（不经过 SeaORM 迁移记账），验证去重与可逆性。
    #[tokio::test]
    async fn up_dedupes_trimmed_duplicates_and_down_restores_tables() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("migration_test.db");
        let db = Database::connect(format!("sqlite:{}?mode=rwc", db_path.display()))
            .await
            .expect("连接数据库失败");

        db.execute_unprepared(OLD_SCHEMA)
            .await
            .expect("建立旧表失败");
        db.execute_unprepared(UP_SQL).await.expect("执行 up 失败");

        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM hotword WHERE scope='role'").await,
            2,
            "角色热词去重后应为两行"
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT weight FROM hotword WHERE scope='role' AND role_id=1"
            )
            .await,
            5,
            "同一角色内仅空白不同的重复行取最大权重"
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT weight FROM hotword WHERE scope='role' AND role_id=2"
            )
            .await,
            3
        );
        assert_eq!(
            scalar::<String>(
                &db,
                "SELECT text FROM hotword WHERE scope='role' AND role_id=1"
            )
            .await,
            "张三",
            "迁移后文本应已 trim"
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM hotword WHERE scope='global'").await,
            3
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT weight FROM hotword WHERE scope='global' AND text='轻越界'"
            )
            .await,
            5,
            "6..49 的越界权重应归一为 5"
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT weight FROM hotword WHERE scope='global' AND text='重越界'"
            )
            .await,
            50,
            ">=50 的越界权重应归一为 50"
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM hotword WHERE scope='library'").await,
            2
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT category_id FROM hotword WHERE scope='library' AND text='钦灵'"
            )
            .await,
            3
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT id FROM hotword_category").await,
            3,
            "分类表重建后应保留原 id"
        );

        db.execute_unprepared(DOWN_SQL)
            .await
            .expect("执行 down 失败");

        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM role_hotword").await,
            2
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM global_hotword").await,
            3
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT weight FROM global_hotword WHERE text='轻越界'").await,
            5,
            "down 后应保留归一后的权重"
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT weight FROM global_hotword WHERE text='重越界'").await,
            50,
            "down 后应保留归一后的权重"
        );
        assert_eq!(
            scalar::<i64>(&db, "SELECT COUNT(*) FROM library_hotword").await,
            2
        );
        assert_eq!(
            scalar::<i64>(
                &db,
                "SELECT category_id FROM library_hotword WHERE text='钦灵'"
            )
            .await,
            3,
            "down 后分类关系应保留"
        );
    }
}
