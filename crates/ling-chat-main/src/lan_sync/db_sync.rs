//! 数据库记录级同步。
//!
//! 替代直接传输 `game_database.db` 二进制文件，
//! 使用 SeaORM 实体导出全表数据为 JSON，暂存并在下次启动时导入目标数据库。

use std::collections::HashSet;
use std::path::Path;

use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, EntityTrait,
    QueryFilter, SqlErr, Statement, TransactionTrait,
};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::entities::hotword::{self, HotwordScope};
use crate::db::entities::hotword_weight::normalize_weight;
use crate::db::entities::{
    adventure_unlock, hotword_category, line, line_perception, memory_bank, role, running_script,
    save,
};
use crate::db::managers::hotword_repo::{MAX_CATEGORIES, MAX_CATEGORY_NAME_CHARS};

use super::messages::DbRecords;
use super::staging::staging_dir;

/// 暂存文件名。
const DB_RECORDS_FILE: &str = "db_records.json";

// ─── 表操作顺序（按外键依赖） ─────────────────────────────────

/// DELETE 顺序：先删子表再删父表，避免 FK 约束冲突。
/// 热词表无外键，位置只为可读性（先词条后分类）。
const DELETE_ORDER: [&str; 9] = [
    "line_perception",
    "memory_bank",
    "line",
    "running_script",
    "adventure_unlock",
    "save",
    "hotword",
    "hotword_category",
    "role",
];

// INSERT 顺序为父表 → 子表；热词走 import_hotwords 做入口归一与归属校验。

/// 导入宏：将 JSON 数组反序列化为 Entity Model，转为 ActiveModel 逐行 INSERT。
///
/// 保留原始主键 ID，实现源端数据镜像。
macro_rules! import_rows {
    ($db:expr, $rows:expr, $entity:ident, $count:expr) => {{
        if !$rows.is_empty() {
            let models: Vec<$entity::Model> = serde_json::from_value(Value::Array($rows.to_vec()))
                .map_err(|e| format!("反序列化 {} 失败: {e}", stringify!($entity)))?;
            for model in models {
                let active: $entity::ActiveModel = model.into();
                $entity::Entity::insert(active)
                    .exec($db)
                    .await
                    .map_err(|e| format!("插入 {} 失败: {e}", stringify!($entity)))?;
                $count += 1;
            }
        }
    }};
}

/// 热词行归一：`weight` 归一到 `[1, 5] ∪ {50}`，`text` 按本地保存的判据 trim。
/// 不合格时返回原因，调用方丢弃该行。
fn normalize_hotword_row(text: &mut String, weight: &mut i32) -> Result<(), String> {
    *weight = normalize_weight(*weight);
    crate::db::managers::hotword_repo::validate_and_trim(text).map_err(|e| e.to_string())
}

/// 归属校验：scope 与 `role_id` / `category_id` 的组合必须与 DDL 约束一致。
fn validate_hotword_ownership(row: &hotword::Model) -> Result<(), String> {
    match row.scope {
        HotwordScope::Role => {
            if row.role_id.is_none() {
                return Err("角色热词缺少 role_id".to_string());
            }
            if row.category_id.is_some() {
                return Err("角色热词不应带 category_id".to_string());
            }
        },
        HotwordScope::Global => {
            if row.role_id.is_some() || row.category_id.is_some() {
                return Err("全局热词不应带 role_id / category_id".to_string());
            }
        },
        HotwordScope::Library => {
            if row.role_id.is_some() {
                return Err("词库词不应带 role_id".to_string());
            }
            if matches!(row.category_id, Some(id) if id <= 0) {
                return Err("词库词分类 id 非法".to_string());
            }
        },
    }
    Ok(())
}

/// 热词单表导入：逐行反序列化、归一、校验归属。
///
/// 单表共享唯一索引，坏行只丢该行并记 warn，不能让一行脏数据拖垮整批镜像。
/// 已见键先去重，避免 trim 后重复的行撞唯一索引导致事务回滚。
async fn import_hotwords(
    db: &impl ConnectionTrait,
    rows: &[Value],
    imported: &mut u64,
) -> Result<(), String> {
    let mut seen: HashSet<(HotwordScope, Option<i32>, Option<i32>, String)> = HashSet::new();

    for row in rows {
        let mut model: hotword::Model = match serde_json::from_value(row.clone()) {
            Ok(model) => model,
            Err(e) => {
                warn!("[LAN] hotword 有一行反序列化失败，已丢弃: {e}");
                continue;
            },
        };
        if let Err(reason) = normalize_hotword_row(&mut model.text, &mut model.weight) {
            warn!("[LAN] hotword 有一行未通过校验，已丢弃: {reason}");
            continue;
        }
        if let Err(reason) = validate_hotword_ownership(&model) {
            warn!("[LAN] hotword 有一行归属非法，已丢弃: {reason}");
            continue;
        }
        let key = (
            model.scope.clone(),
            model.role_id,
            model.category_id,
            model.text.clone(),
        );
        if !seen.insert(key) {
            warn!("[LAN] hotword 有一行与前面的行重复，已丢弃: {}", model.text);
            continue;
        }

        let active: hotword::ActiveModel = model.into();
        match hotword::Entity::insert(active).exec(db).await {
            Ok(_) => *imported += 1,
            Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
                warn!("[LAN] hotword 有一行与已有记录冲突，已丢弃: {e}");
            },
            Err(e) => return Err(format!("插入 hotword 失败: {e}")),
        }
    }
    Ok(())
}

/// 分类单独导入：trim、丢空、超长、重名只丢该行；条数超上限后丢弃剩余。
///
/// 分类名有长度 CHECK，坏行只丢该行，不能拖垮整批镜像。
async fn import_categories(
    db: &impl ConnectionTrait,
    rows: &[Value],
    imported: &mut u64,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let models: Vec<hotword_category::Model> = serde_json::from_value(Value::Array(rows.to_vec()))
        .map_err(|e| format!("反序列化 hotword_category 失败: {e}"))?;
    let mut names: HashSet<String> = HashSet::new();
    for mut model in models {
        if names.len() >= MAX_CATEGORIES {
            tracing::warn!("[LAN] hotword_category 超过 {MAX_CATEGORIES} 条，已丢弃剩余");
            break;
        }
        let name = model.name.trim().to_string();
        let chars = name.chars().count();
        if name.is_empty() || chars > MAX_CATEGORY_NAME_CHARS {
            tracing::warn!("[LAN] hotword_category 有一行未通过校验，已丢弃: {chars} 字符");
            continue;
        }
        if !names.insert(name.clone()) {
            tracing::warn!("[LAN] hotword_category 有一行重名，已丢弃: {name}");
            continue;
        }
        model.name = name;
        let active: hotword_category::ActiveModel = model.into();
        match hotword_category::Entity::insert(active).exec(db).await {
            Ok(_) => *imported += 1,
            Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
                tracing::warn!("[LAN] hotword_category 有一行与已有记录冲突，已丢弃: {e}");
            },
            Err(e) => return Err(format!("插入 hotword_category 失败: {e}")),
        }
    }
    Ok(())
}

// ─── 导出 ─────────────────────────────────────────────────────

/// 导出全部业务表的数据（`DbRecords` 的字段即清单）。
///
/// 通过独立只读连接打开数据库（`mode=ro`），不阻塞主应用的读写连接。
pub async fn export_all_records(data_dir: &Path) -> Result<DbRecords, String> {
    let db_path = data_dir.join("game_database.db");

    if !db_path.exists() {
        return Err("数据库文件不存在，无法导出".to_string());
    }

    let db_url = format!("sqlite:{}?mode=ro", db_path.display());
    let db = Database::connect(&db_url)
        .await
        .map_err(|e| format!("连接数据库失败(只读): {e}"))?;

    let device_id = read_device_id(data_dir).unwrap_or_default();

    info!("开始导出数据库记录...");

    let roles = export_table::<role::Entity, role::Model>(&db, "role").await?;
    let hotwords = export_table::<hotword::Entity, hotword::Model>(&db, "hotword").await?;
    let hotword_categories =
        export_table::<hotword_category::Entity, hotword_category::Model>(&db, "hotword_category")
            .await?;
    let saves = export_table::<save::Entity, save::Model>(&db, "save").await?;
    let running_scripts =
        export_table::<running_script::Entity, running_script::Model>(&db, "running_script")
            .await?;
    let adventure_unlocks =
        export_table::<adventure_unlock::Entity, adventure_unlock::Model>(&db, "adventure_unlock")
            .await?;
    let lines = export_table::<line::Entity, line::Model>(&db, "line").await?;
    let memory_banks =
        export_table::<memory_bank::Entity, memory_bank::Model>(&db, "memory_bank").await?;
    let line_perceptions =
        export_table::<line_perception::Entity, line_perception::Model>(&db, "line_perception")
            .await?;

    let records = DbRecords {
        device_id,
        roles,
        hotwords,
        hotword_categories,
        saves,
        running_scripts,
        adventure_unlocks,
        lines,
        memory_banks,
        line_perceptions,
    };
    info!("数据库导出完成: {} 条记录", records.record_count());

    Ok(records)
}

/// 导出单张表的所有行，序列化为 `Vec<serde_json::Value>`。
async fn export_table<E, M>(db: &DatabaseConnection, name: &str) -> Result<Vec<Value>, String>
where
    E: EntityTrait<Model = M>,
    M: serde::Serialize,
{
    let rows = E::find()
        .all(db)
        .await
        .map_err(|e| format!("查询表 {} 失败: {e}", name))?;

    rows.into_iter()
        .map(|model| {
            serde_json::to_value(&model).map_err(|e| format!("序列化表 {} 的行失败: {e}", name))
        })
        .collect()
}

/// 从磁盘读取设备 ID（仅用于导出标识来源）。
fn read_device_id(data_dir: &Path) -> Option<String> {
    let path = data_dir.join(".lan_sync_device.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("deviceId") // camelCase（DeviceIdentity 序列化格式）
        .or_else(|| v.get("device_id")) // snake_case 兼容
        .and_then(|id| id.as_str())
        .map(String::from)
}

// ─── 暂存 ─────────────────────────────────────────────────────

/// 将数据库记录暂存到 `.lan_sync_staging/db_records.json`。
pub fn stage_db_records(data_dir: &Path, records: &DbRecords) -> Result<(), String> {
    let staging = staging_dir(data_dir);
    std::fs::create_dir_all(&staging).map_err(|e| format!("创建暂存目录失败: {e}"))?;

    let path = staging.join(DB_RECORDS_FILE);
    let json =
        serde_json::to_string_pretty(records).map_err(|e| format!("序列化 DB 记录失败: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("写入暂存 DB 记录失败: {e}"))?;

    info!("数据库记录已暂存: {:?}", path);
    Ok(())
}

/// 从暂存目录加载数据库记录（如果存在）。
pub fn load_staged_db_records(data_dir: &Path) -> Option<DbRecords> {
    let path = staging_dir(data_dir).join(DB_RECORDS_FILE);
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str::<DbRecords>(&json) {
            Ok(records) => Some(records),
            Err(e) => {
                warn!("解析暂存 DB 记录失败: {e}");
                None
            },
        },
        Err(e) => {
            warn!("读取暂存 DB 记录失败: {e}");
            None
        },
    }
}

// ─── 导入 ─────────────────────────────────────────────────────

/// 将暂存的数据库记录应用到主数据库。
///
/// 在 `db::init_db()` 调用之后执行（此时表结构已就绪）。
///
/// 应用失败不上抛：记录 warn、暂存文件改名为 `.bad` 并返回 `Ok(0)`，避免下次启动
/// 重复失败。返回导入的总行数；无暂存记录或应用失败时为 0。
pub async fn apply_staged_db_records(
    db: &DatabaseConnection,
    data_dir: &Path,
) -> Result<u64, String> {
    let records = match load_staged_db_records(data_dir) {
        Some(r) => r,
        None => return Ok(0),
    };

    let staging_path = staging_dir(data_dir).join(DB_RECORDS_FILE);
    let src_id = truncate_str(&records.device_id, 8);
    info!("开始导入数据库记录 (来源: {})...", src_id);

    match apply_records(db, &records).await {
        Ok(imported) => {
            if let Err(e) = std::fs::remove_file(&staging_path) {
                warn!("清理暂存 DB 记录文件失败: {e}");
            }
            info!("数据库记录导入完成: {} 行", imported);
            Ok(imported)
        },
        Err(e) => {
            warn!("应用暂存 DB 记录失败，跳过本次导入: {e}");
            let bad_path = staging_path.with_extension("json.bad");
            if let Err(e) = std::fs::rename(&staging_path, &bad_path) {
                warn!("暂存 DB 记录文件改名失败: {e}");
            } else {
                info!("已保留失败记录: {:?}", bad_path);
            }
            Ok(0)
        },
    }
}

/// 在单个事务中应用记录：按依赖顺序 DELETE 全部行、再重新 INSERT。
///
/// 事务内 `PRAGMA foreign_keys = OFF` 不生效，导入期间外键仍开启，因此
/// DELETE_ORDER 与 INSERT 顺序是硬约束。失败即回滚。
async fn apply_records(db: &DatabaseConnection, records: &DbRecords) -> Result<u64, String> {
    // 在事务中执行全部操作
    let txn = db.begin().await.map_err(|e| format!("开始事务失败: {e}"))?;
    txn.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "PRAGMA foreign_keys = OFF",
    ))
    .await
    .map_err(|e| format!("禁用外键检查失败: {e}"))?;

    let mut imported: u64 = 0;

    // 玩家角色是本地身份卡的投影，不参与整表镜像；先建立远端 id → 本地 id 映射，
    // 供存档与台词里的玩家角色引用翻译。
    let local_players = role::Entity::find()
        .filter(role::Column::RoleType.eq(role::RoleType::Player))
        .all(&txn)
        .await
        .map_err(|e| format!("读取本地玩家角色失败: {e}"))?;
    let local_by_identity: std::collections::HashMap<String, i32> = local_players
        .iter()
        .filter_map(|r| r.identity_id.clone().map(|id| (id, r.id)))
        .collect();
    let mut remote_player_map: std::collections::HashMap<i64, i32> =
        std::collections::HashMap::new();
    let mut remote_player_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for row in &records.roles {
        if row.get("role_type").and_then(Value::as_str) != Some("player") {
            continue;
        }
        let Some(remote_id) = row.get("id").and_then(Value::as_i64) else {
            continue;
        };
        remote_player_ids.insert(remote_id);
        let Some(identity_id) = row.get("identity_id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(local_id) = local_by_identity.get(identity_id) {
            remote_player_map.insert(remote_id, *local_id);
        }
    }
    let mut saves = records.saves.clone();
    for row in &mut saves {
        translate_player_ref(
            row,
            "player_role_id",
            &remote_player_map,
            &remote_player_ids,
            Value::Null,
        );
    }
    let mut lines = records.lines.clone();
    for row in &mut lines {
        translate_player_ref(
            row,
            "sender_role_id",
            &remote_player_map,
            &remote_player_ids,
            Value::from(0),
        );
    }
    let remote_roles: Vec<Value> = records
        .roles
        .iter()
        .filter(|row| row.get("role_type").and_then(Value::as_str) != Some("player"))
        .cloned()
        .collect();

    // ─── DELETE 全部行（子表 → 父表顺序） ────────────────
    for table in DELETE_ORDER {
        let count = if table == "role" {
            txn.execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DELETE FROM role WHERE role_type <> 'player'".to_string(),
            ))
            .await
            .map_err(|e| format!("清空非玩家角色失败: {e}"))?
            .rows_affected()
        } else {
            delete_all(&txn, table).await?
        };
        if count > 0 {
            info!("已清除 {}: {} 行", table, count);
        }
    }

    // ─── INSERT 全部行（父表 → 子表顺序） ────────────────
    import_rows!(&txn, &remote_roles, role, imported);
    import_categories(&txn, &records.hotword_categories, &mut imported).await?;
    import_hotwords(&txn, &records.hotwords, &mut imported).await?;
    import_rows!(&txn, &saves, save, imported);
    import_rows!(&txn, &records.running_scripts, running_script, imported);
    import_rows!(&txn, &records.adventure_unlocks, adventure_unlock, imported);
    import_rows!(&txn, &lines, line, imported);
    import_rows!(&txn, &records.memory_banks, memory_bank, imported);
    import_rows!(&txn, &records.line_perceptions, line_perception, imported);

    // 重新启用外键检查
    txn.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "PRAGMA foreign_keys = ON",
    ))
    .await
    .map_err(|e| format!("启用外键检查失败: {e}"))?;

    txn.commit()
        .await
        .map_err(|e| format!("提交事务失败: {e}"))?;

    Ok(imported)
}

/// 把远端玩家角色 id 翻译成本地 id；远端玩家行不存在时写 fallback。
fn translate_player_ref(
    row: &mut Value,
    field: &str,
    map: &std::collections::HashMap<i64, i32>,
    remote_player_ids: &std::collections::HashSet<i64>,
    fallback: Value,
) {
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    let Some(remote_id) = obj.get(field).and_then(Value::as_i64) else {
        return;
    };
    if !remote_player_ids.contains(&remote_id) {
        return;
    }
    let local = map
        .get(&remote_id)
        .map(|id| Value::from(*id))
        .unwrap_or(fallback);
    obj.insert(field.to_string(), local);
}

/// 删除表中的全部行。
async fn delete_all(db: &impl ConnectionTrait, table: &str) -> Result<u64, String> {
    let sql = format!("DELETE FROM {}", table);
    let result = db
        .execute(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await
        .map_err(|e| format!("清空表 {} 失败: {e}", table))?;
    Ok(result.rows_affected())
}

fn truncate_str(s: &str, max: usize) -> &str {
    let len = s.chars().count();
    if len <= max {
        s
    } else {
        &s[..s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, Set};

    /// 直接把 JSON 写入暂存目录，绕过 `DbRecords` 构造以模拟旧端 payload。
    fn write_staged_payload(data_dir: &Path, payload: &Value) {
        let staging = staging_dir(data_dir);
        std::fs::create_dir_all(&staging).expect("创建暂存目录失败");
        std::fs::write(
            staging.join(DB_RECORDS_FILE),
            serde_json::to_string(payload).expect("序列化 payload 失败"),
        )
        .expect("写入暂存 payload 失败");
    }

    /// 畸形 payload 应用失败时不上抛 `Err`，且暂存文件改名为 `.bad`。
    #[tokio::test]
    async fn malformed_payload_does_not_fail_startup() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let data_dir = tmp.path();

        let db = crate::db::init_db(data_dir)
            .await
            .expect("初始化数据库失败");

        // roles 里 name 类型非法：加载阶段不报错，导入反序列化时失败。
        let records = DbRecords {
            device_id: "test-device".to_string(),
            roles: vec![serde_json::json!({ "id": 7, "name": 123 })],
            saves: vec![],
            running_scripts: vec![],
            adventure_unlocks: vec![],
            lines: vec![],
            memory_banks: vec![],
            line_perceptions: vec![],
            hotwords: vec![],
            hotword_categories: vec![],
        };
        stage_db_records(data_dir, &records).expect("暂存失败");
        assert!(
            load_staged_db_records(data_dir).is_some(),
            "用例前提：payload 应能通过加载阶段"
        );

        let result = apply_staged_db_records(&db, data_dir).await;
        let staging = staging_dir(data_dir);

        assert_eq!(result, Ok(0), "畸形 payload 不应上抛错误");
        assert!(
            !staging.join(DB_RECORDS_FILE).exists(),
            "原暂存文件应已被移走"
        );
        assert!(
            staging.join("db_records.json.bad").exists(),
            "应生成 .bad 文件"
        );
    }

    /// 导出→导入后角色热词必须与源端一致。
    #[tokio::test]
    async fn mirror_includes_hotwords() {
        let src = tempfile::tempdir().expect("创建临时目录失败");
        let src_db = crate::db::init_db(src.path())
            .await
            .expect("初始化源数据库失败");

        role::ActiveModel {
            id: Set(7),
            name: Set("甲".to_string()),
            role_type: Set(role::RoleType::Main),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入角色失败");
        hotword::ActiveModel {
            scope: Set(HotwordScope::Role),
            role_id: Set(Some(7)),
            category_id: Set(None),
            text: Set("张三".to_string()),
            weight: Set(5),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入热词失败");

        let records = export_all_records(src.path()).await.expect("导出失败");
        assert_eq!(records.hotwords.len(), 1, "导出应包含热词");

        let dst = tempfile::tempdir().expect("创建临时目录失败");
        let dst_db = crate::db::init_db(dst.path())
            .await
            .expect("初始化目标数据库失败");
        // 目标端预置旧数据，验证镜像会先清空整张 hotword 表
        hotword::ActiveModel {
            scope: Set(HotwordScope::Global),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("旧词".to_string()),
            weight: Set(4),
            ..Default::default()
        }
        .insert(&dst_db)
        .await
        .expect("预置热词失败");

        stage_db_records(dst.path(), &records).expect("暂存失败");

        let imported = apply_staged_db_records(&dst_db, dst.path())
            .await
            .expect("导入失败");
        assert!(imported >= 2, "至少应导入角色 + 热词，实际 {imported}");

        let hotwords = hotword::Entity::find()
            .all(&dst_db)
            .await
            .expect("查询热词失败");
        assert_eq!(hotwords.len(), 1, "镜像应先清空旧热词，再导入源端热词");
        assert_eq!(hotwords[0].scope, HotwordScope::Role);
        assert_eq!(hotwords[0].role_id, Some(7));
        assert_eq!((hotwords[0].text.as_str(), hotwords[0].weight), ("张三", 5));
    }

    /// 导出→导入后全局热词必须与源端一致。
    #[tokio::test]
    async fn mirror_includes_global_hotwords() {
        let src = tempfile::tempdir().expect("创建临时目录失败");
        let src_db = crate::db::init_db(src.path())
            .await
            .expect("初始化源数据库失败");
        hotword::ActiveModel {
            scope: Set(HotwordScope::Global),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("全局词".to_string()),
            weight: Set(5),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入全局热词失败");

        let records = export_all_records(src.path()).await.expect("导出失败");
        assert_eq!(records.hotwords.len(), 1, "导出应包含全局热词");

        let dst = tempfile::tempdir().expect("创建临时目录失败");
        let dst_db = crate::db::init_db(dst.path())
            .await
            .expect("初始化目标数据库失败");
        // 目标端预置旧数据，验证镜像会先清空该表
        hotword::ActiveModel {
            scope: Set(HotwordScope::Global),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("旧词".to_string()),
            weight: Set(4),
            ..Default::default()
        }
        .insert(&dst_db)
        .await
        .expect("预置全局热词失败");

        stage_db_records(dst.path(), &records).expect("暂存失败");
        let imported = apply_staged_db_records(&dst_db, dst.path())
            .await
            .expect("导入失败");
        assert!(imported >= 1, "至少应导入全局热词，实际 {imported}");

        let hotwords = hotword::Entity::find()
            .all(&dst_db)
            .await
            .expect("查询全局热词失败");
        assert_eq!(hotwords.len(), 1, "镜像应先清空旧全局热词，再导入源端数据");
        assert_eq!(hotwords[0].scope, HotwordScope::Global);
        assert_eq!(hotwords[0].role_id, None);
        assert_eq!(
            (hotwords[0].text.as_str(), hotwords[0].weight),
            ("全局词", 5)
        );
    }

    /// 导出→导入后词库必须与源端一致（含未分类与分类关系）。
    #[tokio::test]
    async fn mirror_includes_hotword_library() {
        let src = tempfile::tempdir().expect("创建临时目录失败");
        let src_db = crate::db::init_db(src.path())
            .await
            .expect("初始化源数据库失败");

        let cat = hotword_category::ActiveModel {
            name: Set("人物".to_string()),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入分类失败");
        hotword::ActiveModel {
            scope: Set(HotwordScope::Library),
            role_id: Set(None),
            category_id: Set(Some(cat.id)),
            text: Set("张三".to_string()),
            weight: Set(5),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入词条失败");
        hotword::ActiveModel {
            scope: Set(HotwordScope::Library),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("通用".to_string()),
            weight: Set(4),
            ..Default::default()
        }
        .insert(&src_db)
        .await
        .expect("插入未分类词条失败");

        let records = export_all_records(src.path()).await.expect("导出失败");
        assert_eq!(records.hotword_categories.len(), 1, "导出应含分类");
        assert_eq!(records.hotwords.len(), 2, "导出应含词条");

        let dst = tempfile::tempdir().expect("创建临时目录失败");
        let dst_db = crate::db::init_db(dst.path())
            .await
            .expect("初始化目标数据库失败");
        // 目标端预置旧数据，验证镜像先清空这两张表
        hotword_category::ActiveModel {
            name: Set("旧分类".to_string()),
            ..Default::default()
        }
        .insert(&dst_db)
        .await
        .expect("预置分类失败");
        hotword::ActiveModel {
            scope: Set(HotwordScope::Library),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("旧词".to_string()),
            weight: Set(4),
            ..Default::default()
        }
        .insert(&dst_db)
        .await
        .expect("预置词库词失败");

        stage_db_records(dst.path(), &records).expect("暂存失败");
        apply_staged_db_records(&dst_db, dst.path())
            .await
            .expect("导入失败");

        let cats = hotword_category::Entity::find()
            .all(&dst_db)
            .await
            .expect("查询分类失败");
        assert_eq!(cats.len(), 1, "镜像应先清空旧分类");
        assert_eq!(cats[0].name, "人物");

        let words = hotword::Entity::find()
            .all(&dst_db)
            .await
            .expect("查询词条失败");
        assert_eq!(words.len(), 2, "镜像应先清空旧词库词");
        let categorized = words
            .iter()
            .find(|row| row.text == "张三")
            .expect("缺少已分类词条");
        assert_eq!(
            categorized.category_id,
            Some(cats[0].id),
            "分类关系随镜像保持"
        );
        let uncategorized = words
            .iter()
            .find(|row| row.text == "通用")
            .expect("缺少未分类词条");
        assert_eq!(uncategorized.category_id, None, "未分类仍是 NULL");
    }

    /// 旧端 payload 没有 hotwords 字段时按空表镜像，而不是解析失败。
    #[tokio::test]
    async fn missing_hotwords_field_mirrors_empty() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let data_dir = tmp.path();
        let db = crate::db::init_db(data_dir)
            .await
            .expect("初始化数据库失败");

        hotword::ActiveModel {
            scope: Set(HotwordScope::Global),
            role_id: Set(None),
            category_id: Set(None),
            text: Set("旧词".to_string()),
            weight: Set(4),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("预置热词失败");

        let payload = serde_json::json!({
            "deviceId": "test-device",
            "roles": [],
            "saves": [],
            "runningScripts": [],
            "adventureUnlocks": [],
            "lines": [],
            "memoryBanks": [],
            "linePerceptions": [],
        });
        write_staged_payload(data_dir, &payload);

        apply_staged_db_records(&db, data_dir)
            .await
            .expect("导入失败");

        let rows = hotword::Entity::find()
            .all(&db)
            .await
            .expect("查询热词失败");
        assert!(rows.is_empty(), "缺少 hotwords 字段时应镜像为空表");
    }

    /// 坏行逐行丢弃，不能因为一行问题让整批镜像回滚。
    #[tokio::test]
    async fn invalid_rows_are_dropped_individually() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let data_dir = tmp.path();
        let db = crate::db::init_db(data_dir)
            .await
            .expect("初始化数据库失败");

        let payload = serde_json::json!({
            "deviceId": "test-device",
            "roles": [],
            "hotwords": [
                { "id": 1, "scope": "global", "role_id": 7, "category_id": null, "text": "带归属", "weight": 4 },
                { "id": 2, "scope": "bogus", "role_id": null, "category_id": null, "text": "坏作用域", "weight": 4 },
                { "id": 3, "scope": "global", "role_id": null, "category_id": null, "text": "好词", "weight": 7 },
                { "id": 4, "scope": "global", "role_id": null, "category_id": null, "text": "好词 ", "weight": 4 }
            ],
            "saves": [],
            "runningScripts": [],
            "adventureUnlocks": [],
            "lines": [],
            "memoryBanks": [],
            "linePerceptions": [],
        });
        write_staged_payload(data_dir, &payload);

        apply_staged_db_records(&db, data_dir)
            .await
            .expect("导入失败");

        let rows = hotword::Entity::find()
            .all(&db)
            .await
            .expect("查询热词失败");
        assert_eq!(rows.len(), 1, "只有归一后合法的唯一行应保留");
        assert_eq!(rows[0].scope, HotwordScope::Global);
        assert_eq!((rows[0].text.as_str(), rows[0].weight), ("好词", 5));
    }
}
