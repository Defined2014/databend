// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::BorrowMut;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::Mutex;

use common_base::infallible::RwLock;
use common_datablocks::DataBlock;
use common_datablocks::HashMethod;
use common_datablocks::HashMethodFixedKeys;
use common_datablocks::HashMethodKind;
use common_datablocks::HashMethodSerializer;
use common_datavalues::Column;
use common_datavalues::ColumnRef;
use common_datavalues::ConstColumn;
use common_datavalues::DataField;
use common_datavalues::DataSchemaRef;
use common_datavalues::DataSchemaRefExt;
use common_datavalues::DataTypeImpl;
use common_datavalues::DataValue;
use common_exception::Result;
use common_hashtable::HashMap;
use common_hashtable::HashTableKeyable;
use primitive_types::U256;
use primitive_types::U512;

use super::ProbeState;
use crate::common::EvalNode;
use crate::common::Evaluator;
use crate::pipelines::new::processors::transforms::hash_join::row::Chunk;
use crate::pipelines::new::processors::transforms::hash_join::row::RowPtr;
use crate::pipelines::new::processors::transforms::hash_join::row::RowSpace;
use crate::pipelines::new::processors::HashJoinState;
use crate::pipelines::transforms::group_by::keys_ref::KeysRef;
use crate::sessions::QueryContext;
use crate::sql::exec::ColumnID;
use crate::sql::exec::PhysicalScalar;
use crate::sql::planner::plans::JoinType;
use crate::sql::IndexType;

pub struct SerializerHashTable {
    pub(crate) hash_table: HashMap<KeysRef, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodSerializer,
}

pub struct KeyU8HashTable {
    pub(crate) hash_table: HashMap<u8, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<u8>,
}

pub struct KeyU16HashTable {
    pub(crate) hash_table: HashMap<u16, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<u16>,
}

pub struct KeyU32HashTable {
    pub(crate) hash_table: HashMap<u32, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<u32>,
}

pub struct KeyU64HashTable {
    pub(crate) hash_table: HashMap<u64, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<u64>,
}

pub struct KeyU128HashTable {
    pub(crate) hash_table: HashMap<u128, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<u128>,
}

pub struct KeyU256HashTable {
    pub(crate) hash_table: HashMap<U256, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<U256>,
}

pub struct KeyU512HashTable {
    pub(crate) hash_table: HashMap<U512, Vec<RowPtr>>,
    pub(crate) hash_method: HashMethodFixedKeys<U512>,
}

pub enum HashTable {
    SerializerHashTable(SerializerHashTable),
    KeyU8HashTable(KeyU8HashTable),
    KeyU16HashTable(KeyU16HashTable),
    KeyU32HashTable(KeyU32HashTable),
    KeyU64HashTable(KeyU64HashTable),
    KeyU128HashTable(KeyU128HashTable),
    KeyU256HashTable(KeyU256HashTable),
    KeyU512HashTable(KeyU512HashTable),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum MarkerKind {
    True,
    False,
    Null,
}

pub struct HashJoinDesc {
    pub(crate) build_keys: Vec<EvalNode<ColumnID>>,
    pub(crate) probe_keys: Vec<EvalNode<ColumnID>>,
    pub(crate) join_type: JoinType,
    pub(crate) other_predicate: Option<EvalNode<ColumnID>>,
    pub(crate) marker: RwLock<Vec<MarkerKind>>,
    pub(crate) marker_index: Option<IndexType>,
}

pub struct JoinHashTable {
    pub(crate) ctx: Arc<QueryContext>,
    /// Reference count
    ref_count: Mutex<usize>,
    is_finished: Mutex<bool>,
    /// A shared big hash table stores all the rows from build side
    pub(crate) hash_table: RwLock<HashTable>,
    pub(crate) row_space: RowSpace,
    pub(crate) hash_join_desc: HashJoinDesc,
}

impl JoinHashTable {
    pub fn create_join_state(
        ctx: Arc<QueryContext>,
        join_type: JoinType,
        build_keys: &[PhysicalScalar],
        probe_keys: &[PhysicalScalar],
        other_predicate: Option<&PhysicalScalar>,
        build_schema: DataSchemaRef,
        marker_index: Option<IndexType>,
    ) -> Result<Arc<JoinHashTable>> {
        let hash_key_types: Vec<DataTypeImpl> =
            build_keys.iter().map(|expr| expr.data_type()).collect();
        let method = DataBlock::choose_hash_method_with_types(&hash_key_types)?;
        let hash_join_desc = HashJoinDesc {
            build_keys: build_keys
                .iter()
                .map(Evaluator::eval_physical_scalar)
                .collect::<Result<_>>()?,
            probe_keys: probe_keys
                .iter()
                .map(Evaluator::eval_physical_scalar)
                .collect::<Result<_>>()?,
            join_type,
            other_predicate: other_predicate
                .map(Evaluator::eval_physical_scalar)
                .transpose()?,
            marker: RwLock::new(vec![]),
            marker_index,
        };
        Ok(match method {
            HashMethodKind::SingleString(_) | HashMethodKind::Serializer(_) => {
                Arc::new(JoinHashTable::try_create(
                    ctx,
                    HashTable::SerializerHashTable(SerializerHashTable {
                        hash_table: HashMap::<KeysRef, Vec<RowPtr>>::create(),
                        hash_method: HashMethodSerializer::default(),
                    }),
                    build_schema,
                    hash_join_desc,
                )?)
            }
            HashMethodKind::KeysU8(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU8HashTable(KeyU8HashTable {
                    hash_table: HashMap::<u8, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU16(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU16HashTable(KeyU16HashTable {
                    hash_table: HashMap::<u16, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU32(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU32HashTable(KeyU32HashTable {
                    hash_table: HashMap::<u32, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU64(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU64HashTable(KeyU64HashTable {
                    hash_table: HashMap::<u64, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU128(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU128HashTable(KeyU128HashTable {
                    hash_table: HashMap::<u128, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU256(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU256HashTable(KeyU256HashTable {
                    hash_table: HashMap::<U256, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
            HashMethodKind::KeysU512(hash_method) => Arc::new(JoinHashTable::try_create(
                ctx,
                HashTable::KeyU512HashTable(KeyU512HashTable {
                    hash_table: HashMap::<U512, Vec<RowPtr>>::create(),
                    hash_method,
                }),
                build_schema,
                hash_join_desc,
            )?),
        })
    }

    pub fn try_create(
        ctx: Arc<QueryContext>,
        hash_table: HashTable,
        mut build_data_schema: DataSchemaRef,
        hash_join_desc: HashJoinDesc,
    ) -> Result<Self> {
        if hash_join_desc.join_type == JoinType::Left {
            let mut nullable_field = Vec::with_capacity(build_data_schema.fields().len());
            for field in build_data_schema.fields().iter() {
                nullable_field.push(DataField::new_nullable(
                    field.name(),
                    field.data_type().clone(),
                ));
            }
            build_data_schema = DataSchemaRefExt::create(nullable_field);
        };
        Ok(Self {
            row_space: RowSpace::new(build_data_schema),
            ref_count: Mutex::new(0),
            is_finished: Mutex::new(false),
            hash_join_desc,
            ctx,
            hash_table: RwLock::new(hash_table),
        })
    }

    // Merge build block and probe block that have the same number of rows
    pub(crate) fn merge_eq_block(
        &self,
        build_block: &DataBlock,
        probe_block: &DataBlock,
    ) -> Result<DataBlock> {
        let mut probe_block = probe_block.clone();
        for (col, field) in build_block
            .columns()
            .iter()
            .zip(build_block.schema().fields().iter())
        {
            probe_block = probe_block.add_column(col.clone(), field.clone())?;
        }
        Ok(probe_block)
    }

    // Merge build block and probe block (1 row block)
    pub(crate) fn merge_with_constant_block(
        &self,
        build_block: &DataBlock,
        probe_block: &DataBlock,
    ) -> Result<DataBlock> {
        let mut replicated_probe_block = DataBlock::empty();
        for (i, col) in probe_block.columns().iter().enumerate() {
            let replicated_col = ConstColumn::new(col.clone(), build_block.num_rows()).arc();

            replicated_probe_block = replicated_probe_block
                .add_column(replicated_col, probe_block.schema().field(i).clone())?;
        }
        for (col, field) in build_block
            .columns()
            .iter()
            .zip(build_block.schema().fields().iter())
        {
            replicated_probe_block =
                replicated_probe_block.add_column(col.clone(), field.clone())?;
        }
        Ok(replicated_probe_block)
    }

    fn probe_cross_join(
        &self,
        input: &DataBlock,
        _probe_state: &mut ProbeState,
    ) -> Result<Vec<DataBlock>> {
        let chunks = self.row_space.chunks.read().unwrap();
        let build_blocks = (*chunks)
            .iter()
            .map(|chunk| chunk.data_block.clone())
            .collect::<Vec<DataBlock>>();
        let build_block = DataBlock::concat_blocks(&build_blocks)?;
        let mut results: Vec<DataBlock> = Vec::with_capacity(input.num_rows());
        for i in 0..input.num_rows() {
            let probe_block = DataBlock::block_take_by_indices(input, &[i as u32])?;
            results.push(self.merge_with_constant_block(&build_block, &probe_block)?);
        }
        Ok(results)
    }

    fn probe_join(
        &self,
        input: &DataBlock,
        probe_state: &mut ProbeState,
    ) -> Result<Vec<DataBlock>> {
        let func_ctx = self.ctx.try_get_function_context()?;
        let probe_keys = self
            .hash_join_desc
            .probe_keys
            .iter()
            .map(|expr| Ok(expr.eval(&func_ctx, input)?.vector().clone()))
            .collect::<Result<Vec<ColumnRef>>>()?;
        let probe_keys = probe_keys.iter().collect::<Vec<&ColumnRef>>();
        match &*self.hash_table.read() {
            HashTable::SerializerHashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                let keys = keys
                    .iter()
                    .map(|key| KeysRef::create(key.as_ptr() as usize, key.len()))
                    .collect();

                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU8HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU16HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU32HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU64HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU128HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU256HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
            HashTable::KeyU512HashTable(table) => {
                let keys = table
                    .hash_method
                    .build_keys(&probe_keys, input.num_rows())?;
                self.result_blocks(&table.hash_table, probe_state, keys, input)
            }
        }
    }
}

impl HashJoinState for JoinHashTable {
    fn build(&self, input: DataBlock) -> Result<()> {
        let func_ctx = self.ctx.try_get_function_context()?;
        let build_cols = self
            .hash_join_desc
            .build_keys
            .iter()
            .map(|expr| Ok(expr.eval(&func_ctx, &input)?.vector().clone()))
            .collect::<Result<Vec<ColumnRef>>>()?;

        match &*self.hash_table.read() {
            HashTable::SerializerHashTable(table) => {
                let mut build_cols_ref = Vec::with_capacity(build_cols.len());
                for build_col in build_cols.iter() {
                    build_cols_ref.push(build_col);
                }
                let build_keys = table
                    .hash_method
                    .build_keys(&build_cols_ref, input.num_rows())?;
                // Save build_keys in row_space to avoid memory leak
                self.row_space.push_keys(input, build_keys)
            }
            _ => self.row_space.push_cols(input, build_cols),
        }
    }

    fn probe(&self, input: &DataBlock, probe_state: &mut ProbeState) -> Result<Vec<DataBlock>> {
        match self.hash_join_desc.join_type {
            JoinType::Inner | JoinType::Semi | JoinType::Anti | JoinType::Left | JoinType::Mark => {
                self.probe_join(input, probe_state)
            }
            JoinType::Cross => self.probe_cross_join(input, probe_state),
            _ => unimplemented!("{} is unimplemented", self.hash_join_desc.join_type),
        }
    }

    fn attach(&self) -> Result<()> {
        let mut count = self.ref_count.lock().unwrap();
        *count += 1;
        Ok(())
    }

    fn detach(&self) -> Result<()> {
        let mut count = self.ref_count.lock().unwrap();
        *count -= 1;
        if *count == 0 {
            self.finish()?;
            let mut is_finished = self.is_finished.lock().unwrap();
            *is_finished = true;
            Ok(())
        } else {
            Ok(())
        }
    }

    fn is_finished(&self) -> Result<bool> {
        Ok(*self.is_finished.lock().unwrap())
    }

    fn finish(&self) -> Result<()> {
        let chunks = self.row_space.chunks.read().unwrap();
        let mut marker = self.hash_join_desc.marker.write();
        for chunk_index in 0..chunks.len() {
            let chunk = &chunks[chunk_index];
            let mut columns = vec![];
            if let Some(cols) = chunk.cols.as_ref() {
                columns = Vec::with_capacity(cols.len());
                for col in cols.iter() {
                    if self.hash_join_desc.join_type == JoinType::Mark {
                        assert_eq!(cols.len(), 1);
                        for row_idx in 0..col.len() {
                            if col.get(row_idx) == DataValue::Null {
                                marker.push(MarkerKind::Null);
                            } else {
                                marker.push(MarkerKind::False);
                            }
                        }
                    }
                    columns.push(col);
                }
            }
            match (*self.hash_table.write()).borrow_mut() {
                HashTable::SerializerHashTable(table) => {
                    if let Some(keys) = chunk.keys.as_ref() {
                        for (row_index, key) in keys.iter().enumerate().take(chunk.num_rows()) {
                            let mut inserted = true;
                            let ptr = RowPtr {
                                chunk_index: chunk_index as u32,
                                row_index: row_index as u32,
                            };
                            let keys_ref = KeysRef::create(key.as_ptr() as usize, key.len());
                            let entity = table.hash_table.insert_key(&keys_ref, &mut inserted);
                            if inserted {
                                entity.set_value(vec![ptr]);
                            } else {
                                entity.get_mut_value().push(ptr);
                            }
                        }
                    }
                }
                HashTable::KeyU8HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU16HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU32HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU64HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU128HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU256HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
                HashTable::KeyU512HashTable(table) => insert_key(
                    &mut table.hash_table,
                    &table.hash_method,
                    chunk,
                    columns,
                    chunk_index,
                )?,
            }
        }

        Ok(())
    }
}

fn insert_key<Key>(
    table: &mut HashMap<Key, Vec<RowPtr>>,
    method: &HashMethodFixedKeys<Key>,
    chunk: &Chunk,
    columns: Vec<&ColumnRef>,
    chunk_index: usize,
) -> Result<()>
where
    Key: HashTableKeyable + Hash + Clone + Default + Debug + 'static,
{
    let build_keys = method.build_keys(&columns, chunk.num_rows())?;
    for (row_index, key) in build_keys.iter().enumerate().take(chunk.num_rows()) {
        let mut inserted = true;
        let ptr = RowPtr {
            chunk_index: chunk_index as u32,
            row_index: row_index as u32,
        };
        let entity = table.insert_key(key, &mut inserted);
        if inserted {
            entity.set_value(vec![ptr]);
        } else {
            entity.get_mut_value().push(ptr);
        }
    }
    Ok(())
}
