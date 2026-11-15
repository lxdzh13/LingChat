use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, EnumIter, DeriveActiveEnum)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(16))")]
pub enum HotwordScope {
    #[sea_orm(string_value = "role")]
    Role,
    #[sea_orm(string_value = "global")]
    Global,
    #[sea_orm(string_value = "library")]
    Library,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "hotword")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub scope: HotwordScope,
    /// 仅 scope='role' 非空；逻辑上指向 role.id。
    pub role_id: Option<i32>,
    /// 仅 scope='library' 可非空；None = 未分类。
    pub category_id: Option<i32>,
    #[sea_orm(column_type = "Text")]
    pub text: String,
    /// 1..=5 或 50，口径见 hotword_weight。
    pub weight: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
