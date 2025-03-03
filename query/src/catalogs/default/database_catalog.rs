//  Copyright 2021 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
//

use std::sync::Arc;

use common_exception::ErrorCode;
use common_exception::Result;
use common_meta_app::schema::CountTablesReply;
use common_meta_app::schema::CountTablesReq;
use common_meta_app::schema::CreateDatabaseReply;
use common_meta_app::schema::CreateDatabaseReq;
use common_meta_app::schema::CreateTableReq;
use common_meta_app::schema::DropDatabaseReq;
use common_meta_app::schema::DropTableReply;
use common_meta_app::schema::DropTableReq;
use common_meta_app::schema::RenameDatabaseReply;
use common_meta_app::schema::RenameDatabaseReq;
use common_meta_app::schema::RenameTableReply;
use common_meta_app::schema::RenameTableReq;
use common_meta_app::schema::TableIdent;
use common_meta_app::schema::TableInfo;
use common_meta_app::schema::TableMeta;
use common_meta_app::schema::UndropDatabaseReply;
use common_meta_app::schema::UndropDatabaseReq;
use common_meta_app::schema::UndropTableReply;
use common_meta_app::schema::UndropTableReq;
use common_meta_app::schema::UpdateTableMetaReply;
use common_meta_app::schema::UpdateTableMetaReq;
use common_meta_app::schema::UpsertTableOptionReply;
use common_meta_app::schema::UpsertTableOptionReq;
use common_meta_types::MetaId;
use common_tracing::tracing;

use crate::catalogs::catalog::Catalog;
use crate::catalogs::default::ImmutableCatalog;
use crate::catalogs::default::MutableCatalog;
use crate::databases::Database;
use crate::storages::StorageDescription;
use crate::storages::Table;
use crate::table_functions::TableArgs;
use crate::table_functions::TableFunction;
use crate::table_functions::TableFunctionFactory;
use crate::Config;

/// Combine two catalogs together
/// - read/search like operations are always performed at
///   upper layer first, and bottom layer later(if necessary)
/// - metadata are written to the bottom layer
#[derive(Clone)]
pub struct DatabaseCatalog {
    /// the upper layer, read only
    immutable_catalog: Arc<dyn Catalog>,
    /// bottom layer, writing goes here
    mutable_catalog: Arc<dyn Catalog>,
    /// table function engine factories
    table_function_factory: Arc<TableFunctionFactory>,
}

impl DatabaseCatalog {
    pub fn create(
        immutable_catalog: Arc<dyn Catalog>,
        mutable_catalog: Arc<dyn Catalog>,
        table_function_factory: Arc<TableFunctionFactory>,
    ) -> Self {
        Self {
            immutable_catalog,
            mutable_catalog,
            table_function_factory,
        }
    }

    pub async fn try_create_with_config(conf: Config) -> Result<DatabaseCatalog> {
        let immutable_catalog = ImmutableCatalog::try_create_with_config(&conf).await?;
        let mutable_catalog = MutableCatalog::try_create_with_config(conf).await?;
        let table_function_factory = TableFunctionFactory::create();
        let res = DatabaseCatalog::create(
            Arc::new(immutable_catalog),
            Arc::new(mutable_catalog),
            Arc::new(table_function_factory),
        );
        Ok(res)
    }

    pub fn is_case_insensitive_db(db: &str) -> bool {
        db.to_uppercase() == "INFORMATION_SCHEMA"
    }
}

#[async_trait::async_trait]
impl Catalog for DatabaseCatalog {
    async fn get_database(&self, tenant: &str, db_name: &str) -> Result<Arc<dyn Database>> {
        if tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while get database)",
            ));
        }

        let db_name = if Self::is_case_insensitive_db(db_name) {
            db_name.to_uppercase()
        } else {
            db_name.to_string()
        };

        let r = self.immutable_catalog.get_database(tenant, &db_name).await;
        match r {
            Err(e) => {
                if e.code() == ErrorCode::unknown_database_code() {
                    self.mutable_catalog.get_database(tenant, &db_name).await
                } else {
                    Err(e)
                }
            }
            Ok(db) => Ok(db),
        }
    }

    async fn list_databases(&self, tenant: &str) -> Result<Vec<Arc<dyn Database>>> {
        if tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while list databases)",
            ));
        }

        let mut dbs = self.immutable_catalog.list_databases(tenant).await?;
        let mut other = self.mutable_catalog.list_databases(tenant).await?;
        dbs.append(&mut other);
        Ok(dbs)
    }

    async fn create_database(&self, req: CreateDatabaseReq) -> Result<CreateDatabaseReply> {
        if req.name_ident.tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while create database)",
            ));
        }
        tracing::info!("Create database from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(&req.name_ident.tenant, &req.name_ident.db_name)
            .await?
        {
            return Err(ErrorCode::DatabaseAlreadyExists(format!(
                "{} database exists",
                req.name_ident.db_name
            )));
        }
        // create db in BOTTOM layer only
        self.mutable_catalog.create_database(req).await
    }

    async fn drop_database(&self, req: DropDatabaseReq) -> Result<()> {
        if req.name_ident.tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while drop database)",
            ));
        }
        tracing::info!("Drop database from req:{:?}", req);

        // drop db in BOTTOM layer only
        if self
            .immutable_catalog
            .exists_database(&req.name_ident.tenant, &req.name_ident.db_name)
            .await?
        {
            return self.immutable_catalog.drop_database(req).await;
        }
        self.mutable_catalog.drop_database(req).await
    }

    async fn rename_database(&self, req: RenameDatabaseReq) -> Result<RenameDatabaseReply> {
        if req.name_ident.tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while rename database)",
            ));
        }
        tracing::info!("Rename table from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(&req.name_ident.tenant, &req.name_ident.db_name)
            .await?
            || self
                .immutable_catalog
                .exists_database(&req.name_ident.tenant, &req.new_db_name)
                .await?
        {
            return self.immutable_catalog.rename_database(req).await;
        }

        self.mutable_catalog.rename_database(req).await
    }

    fn get_table_by_info(&self, table_info: &TableInfo) -> Result<Arc<dyn Table>> {
        let res = self.immutable_catalog.get_table_by_info(table_info);
        match res {
            Ok(t) => Ok(t),
            Err(e) => {
                if e.code() == ErrorCode::unknown_table_code() {
                    self.mutable_catalog.get_table_by_info(table_info)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn get_table_meta_by_id(&self, table_id: MetaId) -> Result<(TableIdent, Arc<TableMeta>)> {
        let res = self.immutable_catalog.get_table_meta_by_id(table_id).await;

        if let Ok(x) = res {
            Ok(x)
        } else {
            self.mutable_catalog.get_table_meta_by_id(table_id).await
        }
    }

    async fn get_table(
        &self,
        tenant: &str,
        db_name: &str,
        table_name: &str,
    ) -> Result<Arc<dyn Table>> {
        if tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while get table)",
            ));
        }

        let (db_name, table_name) = if Self::is_case_insensitive_db(db_name) {
            (db_name.to_uppercase(), table_name.to_uppercase())
        } else {
            (db_name.to_string(), table_name.to_string())
        };

        let res = self
            .immutable_catalog
            .get_table(tenant, &db_name, &table_name)
            .await;
        match res {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.code() == ErrorCode::UnknownDatabaseCode() {
                    self.mutable_catalog
                        .get_table(tenant, &db_name, &table_name)
                        .await
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn list_tables(&self, tenant: &str, db_name: &str) -> Result<Vec<Arc<dyn Table>>> {
        if tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while list tables)",
            ));
        }

        let db_name = if Self::is_case_insensitive_db(db_name) {
            db_name.to_uppercase()
        } else {
            db_name.to_string()
        };

        let r = self.immutable_catalog.list_tables(tenant, &db_name).await;
        match r {
            Ok(x) => Ok(x),
            Err(e) => {
                if e.code() == ErrorCode::UnknownDatabaseCode() {
                    self.mutable_catalog.list_tables(tenant, &db_name).await
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn list_tables_history(
        &self,
        tenant: &str,
        db_name: &str,
    ) -> Result<Vec<Arc<dyn Table>>> {
        if tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while list tables)",
            ));
        }

        let db_name = if Self::is_case_insensitive_db(db_name) {
            db_name.to_uppercase()
        } else {
            db_name.to_string()
        };

        let r = self
            .immutable_catalog
            .list_tables_history(tenant, &db_name)
            .await;
        match r {
            Ok(x) => Ok(x),
            Err(e) => {
                if e.code() == ErrorCode::UnknownDatabaseCode() {
                    self.mutable_catalog
                        .list_tables_history(tenant, &db_name)
                        .await
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn create_table(&self, req: CreateTableReq) -> Result<()> {
        if req.tenant().is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while create table)",
            ));
        }
        tracing::info!("Create table from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(req.tenant(), req.db_name())
            .await?
        {
            return self.immutable_catalog.create_table(req).await;
        }
        self.mutable_catalog.create_table(req).await
    }

    async fn drop_table(&self, req: DropTableReq) -> Result<DropTableReply> {
        if req.tenant().is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while drop table)",
            ));
        }
        tracing::info!("Drop table from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(req.tenant(), req.db_name())
            .await?
        {
            return self.immutable_catalog.drop_table(req).await;
        }
        self.mutable_catalog.drop_table(req).await
    }

    async fn undrop_table(&self, req: UndropTableReq) -> Result<UndropTableReply> {
        if req.tenant().is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while undrop table)",
            ));
        }
        tracing::info!("Undrop table from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(req.tenant(), req.db_name())
            .await?
        {
            return self.immutable_catalog.undrop_table(req).await;
        }
        self.mutable_catalog.undrop_table(req).await
    }

    async fn undrop_database(&self, req: UndropDatabaseReq) -> Result<UndropDatabaseReply> {
        if req.tenant().is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while undrop database)",
            ));
        }
        tracing::info!("Undrop database from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(req.tenant(), req.db_name())
            .await?
        {
            return self.immutable_catalog.undrop_database(req).await;
        }
        self.mutable_catalog.undrop_database(req).await
    }

    async fn rename_table(&self, req: RenameTableReq) -> Result<RenameTableReply> {
        if req.tenant().is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while rename table)",
            ));
        }
        tracing::info!("Rename table from req:{:?}", req);

        if self
            .immutable_catalog
            .exists_database(req.tenant(), req.db_name())
            .await?
            || self
                .immutable_catalog
                .exists_database(req.tenant(), &req.new_db_name)
                .await?
        {
            return Err(ErrorCode::UnImplement(
                "Cannot rename table from(to) system databases",
            ));
        }

        self.mutable_catalog.rename_table(req).await
    }

    async fn count_tables(&self, req: CountTablesReq) -> Result<CountTablesReply> {
        if req.tenant.is_empty() {
            return Err(ErrorCode::TenantIsEmpty(
                "Tenant can not empty(while count tables)",
            ));
        }

        let res = self.mutable_catalog.count_tables(req).await?;

        Ok(res)
    }

    async fn upsert_table_option(
        &self,
        req: UpsertTableOptionReq,
    ) -> Result<UpsertTableOptionReply> {
        self.mutable_catalog.upsert_table_option(req).await
    }

    async fn update_table_meta(&self, req: UpdateTableMetaReq) -> Result<UpdateTableMetaReply> {
        self.mutable_catalog.update_table_meta(req).await
    }

    fn get_table_function(
        &self,
        func_name: &str,
        tbl_args: TableArgs,
    ) -> Result<Arc<dyn TableFunction>> {
        self.table_function_factory.get(func_name, tbl_args)
    }

    fn get_table_engines(&self) -> Vec<StorageDescription> {
        // only return mutable_catalog storage table engines
        self.mutable_catalog.get_table_engines()
    }
}
