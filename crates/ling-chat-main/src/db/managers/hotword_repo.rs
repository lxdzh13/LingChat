use std::collections::HashMap;
use std::hash::Hash;

use anyhow::anyhow;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::db::entities::hotword::{self, HotwordScope};
use crate::db::entities::hotword_category;
use crate::db::entities::hotword_weight::normalize_weight;

/// 每作用域条数上限（角色与全局同一口径）。
const MAX_ROWS_PER_ROLE: usize = 200;
/// 含非 ASCII 时按总字符数计，纯 ASCII 时按空格切分的片段数计；两者不是同一个上限。
const MAX_TEXT_CHARS: usize = 15;
const MAX_TEXT_SEGMENTS: usize = 7;

/// `trim` 后判空并校验长度，校验的是 `trim` 后的值。
///
/// 必须在调用方做（而不是塞进 [`validate_text_len`]）：`text` 参与归并/唯一键
///（词库还含分类），未 `trim` 就入库会让同一个词出现两个 key。
pub(crate) fn validate_and_trim(text: &mut String) -> Result<(), anyhow::Error> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("热词为空"));
    }
    // 热词会拼进本地 ASR 的 prompt，换行/制表等控制字符不应进入
    if trimmed.chars().any(char::is_control) {
        return Err(anyhow!("热词包含控制字符"));
    }
    validate_text_len(trimmed)?;
    if trimmed.len() != text.len() {
        *text = trimmed.to_string();
    }
    Ok(())
}

/// 按是否含非 ASCII 分流校验单条词条长度，`lan_sync` 镜像导入复用同一判据。
///
/// 调用方须先 `trim` 并丢弃空串：本函数只判长度，纯空白串会放行。
pub fn validate_text_len(text: &str) -> Result<(), anyhow::Error> {
    if text.is_ascii() {
        let segments = text.split_whitespace().count();
        if segments > MAX_TEXT_SEGMENTS {
            return Err(anyhow!(
                "热词超长：{segments} 个片段 > {MAX_TEXT_SEGMENTS}（纯 ASCII 按空格切分计）"
            ));
        }
        return Ok(());
    }
    let chars = text.chars().count();
    if chars > MAX_TEXT_CHARS {
        return Err(anyhow!("热词超长：{chars} 字符 > {MAX_TEXT_CHARS}"));
    }
    Ok(())
}

/// 词库条数上限：词库不上行给 provider，上限只为让 LAN payload 和 UI 规模有界。
pub const MAX_ROWS_PER_LIBRARY: usize = 2000;
/// 词库分类数上限。
pub const MAX_CATEGORIES: usize = 100;
/// 分类名字符数上限（按字符计）。
pub const MAX_CATEGORY_NAME_CHARS: usize = 30;

/// 归并内核的单行产出；`first_index` 是该行在入参中的首现下标
/// （词库侧据此取回 `category_index`，角色/全局侧忽略）。
pub struct MergedRow {
    pub first_index: usize,
    pub text: String,
    pub weight: i32,
}

/// 校验并归并：trim、丢空、查超限/超长，按 `key_of` 归并取 max、保留首现位置。
/// 角色 / 全局 / 词库三处共用，只差 `max_rows` 与归并键。
fn validate_and_merge_by<K, F>(
    rows: &[(String, i32)],
    max_rows: usize,
    key_of: F,
) -> Result<Vec<MergedRow>, anyhow::Error>
where
    K: Eq + Hash,
    F: Fn(usize, &str) -> K,
{
    if rows.len() > max_rows {
        return Err(anyhow!("热词条数超限：{} > {max_rows}", rows.len()));
    }

    let mut merged: Vec<MergedRow> = Vec::new();
    let mut positions: HashMap<K, usize> = HashMap::new();
    for (index, (text, weight)) in rows.iter().enumerate() {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        validate_text_len(text)?;
        let weight = normalize_weight(*weight);
        // 归并键与唯一键同形，避免入参自撞唯一键让事务失败。
        let key = key_of(index, text);
        if let Some(&position) = positions.get(&key) {
            merged[position].weight = merged[position].weight.max(weight);
        } else {
            positions.insert(key, merged.len());
            merged.push(MergedRow {
                first_index: index,
                text: text.to_string(),
                weight,
            });
        }
    }
    Ok(merged)
}

/// 角色 / 全局的归并键：同一作用域内 `text` 唯一。
fn validate_and_merge(
    rows: &[(String, i32)],
    max_rows: usize,
) -> Result<Vec<MergedRow>, anyhow::Error> {
    validate_and_merge_by(rows, max_rows, |_, text| text.to_string())
}

pub struct HotwordRepo;

impl HotwordRepo {
    /// 按 id 升序 = 插入顺序（即场景热词的组内顺序）。
    pub async fn list_by_role<C: ConnectionTrait>(
        db: &C,
        role_id: i32,
    ) -> Result<Vec<hotword::Model>, anyhow::Error> {
        hotword::Entity::find()
            .filter(hotword::Column::Scope.eq(HotwordScope::Role))
            .filter(hotword::Column::RoleId.eq(role_id))
            .order_by_asc(hotword::Column::Id)
            .all(db)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// 与 `list_by_role` 同一口径（按 id 升序）；空入参返回空。
    pub async fn list_by_roles<C: ConnectionTrait>(
        db: &C,
        role_ids: &[i32],
    ) -> Result<Vec<hotword::Model>, anyhow::Error> {
        if role_ids.is_empty() {
            return Ok(Vec::new());
        }
        hotword::Entity::find()
            .filter(hotword::Column::Scope.eq(HotwordScope::Role))
            .filter(hotword::Column::RoleId.is_in(role_ids.iter().copied()))
            .order_by_asc(hotword::Column::Id)
            .all(db)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// 整表替换（delete + insert），入参按 trim 后的 `text` 归并取 max weight。
    /// 全部校验通过后才动库。事务由调用方开启，有事务时必须传 `&txn`
    /// （SQLite 单连接池，传 `&db` 会自锁）。
    pub async fn replace_for_role<C: ConnectionTrait>(
        db: &C,
        role_id: i32,
        rows: &[(String, i32)],
    ) -> Result<(), anyhow::Error> {
        let merged = validate_and_merge(rows, MAX_ROWS_PER_ROLE)?;
        hotword::Entity::delete_many()
            .filter(hotword::Column::Scope.eq(HotwordScope::Role))
            .filter(hotword::Column::RoleId.eq(role_id))
            .exec(db)
            .await
            .map_err(|e| anyhow!(e))?;
        for MergedRow { text, weight, .. } in merged {
            hotword::ActiveModel {
                scope: Set(HotwordScope::Role),
                role_id: Set(Some(role_id)),
                category_id: Set(None),
                text: Set(text),
                weight: Set(weight),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    pub async fn replace_global<C: ConnectionTrait>(
        db: &C,
        rows: &[(String, i32)],
    ) -> Result<(), anyhow::Error> {
        let merged = validate_and_merge(rows, MAX_ROWS_PER_ROLE)?;
        hotword::Entity::delete_many()
            .filter(hotword::Column::Scope.eq(HotwordScope::Global))
            .exec(db)
            .await
            .map_err(|e| anyhow!(e))?;
        for MergedRow { text, weight, .. } in merged {
            hotword::ActiveModel {
                scope: Set(HotwordScope::Global),
                role_id: Set(None),
                category_id: Set(None),
                text: Set(text),
                weight: Set(weight),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    pub async fn list_global<C: ConnectionTrait>(
        db: &C,
    ) -> Result<Vec<hotword::Model>, anyhow::Error> {
        hotword::Entity::find()
            .filter(hotword::Column::Scope.eq(HotwordScope::Global))
            .order_by_asc(hotword::Column::Id)
            .all(db)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// 词库的分类与词条，皆按 id 升序 = 插入顺序（整表替换按序插入）。
    pub async fn list_library<C: ConnectionTrait>(
        db: &C,
    ) -> Result<(Vec<hotword_category::Model>, Vec<hotword::Model>), anyhow::Error> {
        let categories = hotword_category::Entity::find()
            .order_by_asc(hotword_category::Column::Id)
            .all(db)
            .await
            .map_err(|e| anyhow!(e))?;
        let words = hotword::Entity::find()
            .filter(hotword::Column::Scope.eq(HotwordScope::Library))
            .order_by_asc(hotword::Column::Id)
            .all(db)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok((categories, words))
    }

    /// 词库整表替换（两表一起重建）。`categories` 的顺序 = 插入顺序，
    /// `words` 的 `category_index` 是 `categories` 的下标（None = 未分类）。
    /// 全部校验通过后才动库；事务由调用方开启，`&txn` 的原因见 `replace_for_role`。
    pub async fn replace_library<C: ConnectionTrait>(
        db: &C,
        categories: &[String],
        words: &[(Option<usize>, String, i32)],
    ) -> Result<(), anyhow::Error> {
        if categories.len() > MAX_CATEGORIES {
            return Err(anyhow!(
                "分类数超限：{} > {MAX_CATEGORIES}",
                categories.len()
            ));
        }
        let mut names: Vec<String> = Vec::with_capacity(categories.len());
        for name in categories {
            let name = name.trim();
            if name.is_empty() {
                return Err(anyhow!("分类名不能为空"));
            }
            let chars = name.chars().count();
            if chars > MAX_CATEGORY_NAME_CHARS {
                return Err(anyhow!(
                    "分类名超长：{chars} 字符 > {MAX_CATEGORY_NAME_CHARS}"
                ));
            }
            // 与 DB 的 UNIQUE(name)（BINARY）同口径：trim 后精确比较
            if names.iter().any(|n| n == name) {
                return Err(anyhow!("分类名重复：{name}"));
            }
            names.push(name.to_string());
        }

        for (index, _, _) in words.iter() {
            if let Some(index) = index {
                if *index >= names.len() {
                    return Err(anyhow!("词条的分类下标越界：{index} >= {}", names.len()));
                }
            }
        }

        let projection: Vec<(String, i32)> = words
            .iter()
            .map(|(_, text, weight)| (text.clone(), *weight))
            .collect();
        // 词库按 (分类, text) 归并：同词可同时存在于不同分类，同分类内仍唯一。
        let merged = validate_and_merge_by(&projection, MAX_ROWS_PER_LIBRARY, |index, text| {
            (words[index].0, text.to_string())
        })?;

        hotword::Entity::delete_many()
            .filter(hotword::Column::Scope.eq(HotwordScope::Library))
            .exec(db)
            .await
            .map_err(|e| anyhow!(e))?;
        hotword_category::Entity::delete_many()
            .exec(db)
            .await
            .map_err(|e| anyhow!(e))?;

        // 按序插分类并收集新 id（AUTOINCREMENT 重分配，无外部引用者）
        let mut ids: Vec<i32> = Vec::with_capacity(names.len());
        for name in names {
            let inserted = hotword_category::ActiveModel {
                name: Set(name),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| anyhow!(e))?;
            ids.push(inserted.id);
        }

        // 归并后按首现下标取回原入参的分类（越界已在上方校验）
        for row in merged {
            let category_id = words[row.first_index].0.map(|index| ids[index]);
            hotword::ActiveModel {
                scope: Set(HotwordScope::Library),
                role_id: Set(None),
                category_id: Set(category_id),
                text: Set(row.text),
                weight: Set(row.weight),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::entities::hotword::{self, HotwordScope};
    use sea_orm::{ActiveModelTrait, Set};

    async fn test_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db = crate::db::init_db(dir.path())
            .await
            .expect("初始化数据库失败");
        (dir, db)
    }

    /// 同名热词分属不同分类时必须各存一行，不能被 `text` 归并掉。
    #[tokio::test]
    async fn library_keeps_same_text_in_different_categories() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_library(
            &db,
            &["人物".to_string(), "产品".to_string()],
            &[
                (Some(0), "钦灵".to_string(), 4),
                (Some(1), "钦灵".to_string(), 5),
            ],
        )
        .await
        .expect("保存失败");

        let (categories, words) = HotwordRepo::list_library(&db).await.expect("读取失败");
        assert_eq!(words.len(), 2, "不同分类的同名词条应各存一行");
        assert_eq!(words[0].category_id, Some(categories[0].id));
        assert_eq!(words[1].category_id, Some(categories[1].id));
    }

    /// 同一分类内的同名词仍按原语义归并取最大权重。
    #[tokio::test]
    async fn library_merges_same_text_within_one_category() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_library(
            &db,
            &["人物".to_string()],
            &[
                (Some(0), "钦灵".to_string(), 4),
                (Some(0), "钦灵".to_string(), 5),
            ],
        )
        .await
        .expect("保存失败");

        let (_, words) = HotwordRepo::list_library(&db).await.expect("读取失败");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].weight, 5);
    }

    /// 未分类之间同样保持唯一；SQLite 的复合唯一索引对 NULL 不做约束，需单独保证。
    #[tokio::test]
    async fn library_merges_same_text_when_uncategorized() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_library(
            &db,
            &[],
            &[(None, "通用".to_string(), 4), (None, "通用".to_string(), 5)],
        )
        .await
        .expect("保存失败");

        let (_, words) = HotwordRepo::list_library(&db).await.expect("读取失败");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].weight, 5);
    }

    /// 同名热词分属不同角色时各存一行，互不覆盖。
    #[tokio::test]
    async fn role_scopes_are_isolated() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_for_role(&db, 1, &[("同名".to_string(), 4)])
            .await
            .expect("保存失败");
        HotwordRepo::replace_for_role(&db, 2, &[("同名".to_string(), 5)])
            .await
            .expect("保存失败");

        let role_one = HotwordRepo::list_by_role(&db, 1).await.expect("读取失败");
        let role_two = HotwordRepo::list_by_role(&db, 2).await.expect("读取失败");
        assert_eq!(role_one.len(), 1);
        assert_eq!(role_two.len(), 1);
        assert_eq!(role_one[0].weight, 4);
        assert_eq!(role_two[0].weight, 5);
        assert_eq!(role_one[0].scope, HotwordScope::Role);
    }

    /// 全局词与未分类词库词的空值形态相同，只能靠 scope 区分，必须能共存。
    #[tokio::test]
    async fn global_and_uncategorized_library_share_text() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_global(&db, &[("通用".to_string(), 4)])
            .await
            .expect("保存失败");
        HotwordRepo::replace_library(&db, &[], &[(None, "通用".to_string(), 5)])
            .await
            .expect("保存失败");

        let global = HotwordRepo::list_global(&db).await.expect("读取失败");
        let (_, words) = HotwordRepo::list_library(&db).await.expect("读取失败");
        assert_eq!(global.len(), 1);
        assert_eq!(words.len(), 1);
        assert_eq!(global[0].scope, HotwordScope::Global);
        assert_eq!(global[0].weight, 4);
        assert_eq!(words[0].scope, HotwordScope::Library);
        assert_eq!(words[0].weight, 5);
    }

    /// 三个读取入口都必须固定各自的 scope，不能读到别的作用域。
    #[tokio::test]
    async fn all_reads_fix_their_scope() {
        let (_dir, db) = test_db().await;
        HotwordRepo::replace_for_role(&db, 1, &[("角色词".to_string(), 4)])
            .await
            .expect("保存失败");
        HotwordRepo::replace_global(&db, &[("全局词".to_string(), 4)])
            .await
            .expect("保存失败");
        HotwordRepo::replace_library(&db, &[], &[(None, "词库词".to_string(), 4)])
            .await
            .expect("保存失败");

        let by_roles = HotwordRepo::list_by_roles(&db, &[1])
            .await
            .expect("读取失败");
        assert_eq!(by_roles.len(), 1);
        assert_eq!(by_roles[0].text, "角色词");
        assert_eq!(by_roles[0].scope, HotwordScope::Role);

        let global = HotwordRepo::list_global(&db).await.expect("读取失败");
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].text, "全局词");
        assert_eq!(global[0].scope, HotwordScope::Global);

        let (_, words) = HotwordRepo::list_library(&db).await.expect("读取失败");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "词库词");
        assert_eq!(words[0].scope, HotwordScope::Library);
    }

    /// scope 与归属列、权重的非法组合由 DDL 拒绝。
    #[tokio::test]
    async fn invalid_scope_assignment_or_weight_is_rejected() {
        let (_dir, db) = test_db().await;

        let cases = [
            (HotwordScope::Role, None, None, 4, "角色词缺少 role_id"),
            (HotwordScope::Global, Some(1), None, 4, "全局词带 role_id"),
            (HotwordScope::Library, Some(1), None, 4, "词库词带 role_id"),
            (HotwordScope::Global, None, None, 7, "非法权重"),
        ];
        for (index, (scope, role_id, category_id, weight, reason)) in cases.into_iter().enumerate()
        {
            let result = hotword::ActiveModel {
                scope: Set(scope),
                role_id: Set(role_id),
                category_id: Set(category_id),
                text: Set(format!("非法{index}")),
                weight: Set(weight),
                ..Default::default()
            }
            .insert(&db)
            .await;
            assert!(result.is_err(), "{reason} 应被拒绝，实际写入成功");
        }
    }
}
