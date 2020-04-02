// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, VecDeque};
use crate::{
	error::{Error, Result},
	table::{TableId as ValueTableId, ValueTable, Key, Value, Address},
	log::{Log, LogReader, LogWriter, LogAction},
	display::hex,
	index::{IndexTable, TableId as IndexTableId,
		PlanOutcome, RebalanceProgress, Entry as IndexEntry},
};

const START_BITS: u8 = 16;
const MAX_REBALANCE_BATCH: u32 = 65536;

pub type ColId = u8;

pub struct Column {
	// Ordered by value size.
	index: IndexTable,
	rebalancing: VecDeque<IndexTable>,
	rebalance_progress: u64,
	path: std::path::PathBuf,
	value_tables: [ValueTable; 15],
	// TODO: make these private
	pub blobs: HashMap<Key, Value>,
	pub histogram: std::collections::BTreeMap<u64, u64>,
}

impl Column {
	pub fn get(&self, key: &Key, log: &Log) -> Result<Option<Value>> {
		let entry = self.index.get(key, log);
		if !entry.is_empty() {
			return self.get_entry_value(key, entry, log);
		}
		for r in &self.rebalancing {
			let entry = r.get(key, log);
			if !entry.is_empty() {
				return self.get_entry_value(key, entry, log);
			}
		}
		Ok(None)
	}

	fn get_entry_value(&self, key: &Key, entry: IndexEntry, log: &Log) -> Result<Option<Value>> {
		let size_tier = entry.address().size_tier();
		if size_tier == 15 {
			return Ok(self.blobs.get(key).cloned())
		}
		self.value_tables[size_tier as usize].get(key, entry.address().offset(), log)
	}

	pub fn open(col: ColId, path: &std::path::Path) -> Result<Column> {
		let (index, rebalancing) = Self::open_index(path, col)?;
		Ok(Column {
			index,
			rebalancing,
			rebalance_progress: 0,
			value_tables: [
				Self::open_table(path, col, 0, 64)?,
				Self::open_table(path, col, 1, 96)?,
				Self::open_table(path, col, 2, 128)?,
				Self::open_table(path, col, 3, 192)?,
				Self::open_table(path, col, 4, 256)?,
				Self::open_table(path, col, 5, 320)?,
				Self::open_table(path, col, 6, 512)?,
				Self::open_table(path, col, 7, 768)?,
				Self::open_table(path, col, 8, 1024)?,
				Self::open_table(path, col, 9, 1536)?,
				Self::open_table(path, col, 10, 2048)?,
				Self::open_table(path, col, 11, 3072)?,
				Self::open_table(path, col, 12, 4096)?,
				Self::open_table(path, col, 13, 8192)?,
				Self::open_table(path, col, 14, 16384)?,
			],
			blobs: HashMap::new(),
			path: path.into(),
			histogram: Default::default(),
		})
	}

	fn open_index(path: &std::path::Path, col: ColId) -> Result<(IndexTable, VecDeque<IndexTable>)> {
		let mut rebalancing = VecDeque::new();
		let mut top = None;
		for bits in (START_BITS .. 65).rev() {
			let id = IndexTableId::new(col, bits);
			if let Some(table) = IndexTable::open_existing(path, id)? {
				if top.is_none() {
					top = Some(table);
				} else {
					rebalancing.push_front(table);
				}
			}
		}
		let table = match top {
			Some(table) => table,
			None => IndexTable::create_new(path, IndexTableId::new(col, START_BITS)),
		};
		Ok((table, rebalancing))
	}

	fn open_table(path: &std::path::Path, col: ColId, tier: u8, entry_size: u16) -> Result<ValueTable> {
		let id = ValueTableId::new(col, tier);
		ValueTable::open(path, id, entry_size)
	}

	fn trigger_rebalance(&mut self) {
		log::info!(
			target: "parity-db",
			"Started index rebalance {} at {}/{} full",
			self.index.id,
			self.index.entries(),
			self.index.id.total_entries(),
		);
			// Start rebalance
		let new_index_id = IndexTableId::new(
			self.index.id.col(),
			self.index.id.index_bits() + 1
		);
		let new_table = IndexTable::create_new(self.path.as_path(), new_index_id);
		let old_table = std::mem::replace(&mut self.index, new_table);
		self.rebalancing.push_back(old_table);
	}

	pub fn write_plan(&mut self, key: &Key, value: &Option<Value>, log: &mut LogWriter) -> Result<()> {
		//TODO: return sub-chunk position in index.get
		if let &Some(ref val) = value {
			*self.histogram.entry(val.len() as u64).or_default() += 1;
			let target_tier = self.value_tables.iter().position(|t| val.len() <= t.value_size() as usize);
			let target_tier = match target_tier {
				Some(tier) => tier as usize,
				None => {
					self.blobs.insert(*key, val.clone());
					return Ok(());
				}
			};

			let existing_entry = self.index.get_planned(key, log);
			if !existing_entry.is_empty() {
				let existing_address = existing_entry.address();
				let existing_tier = existing_address.size_tier() as usize;
				let replace = self.value_tables[existing_tier].has_key_at(existing_address.offset(), &key, log)?;
				if replace {
					if existing_tier == target_tier {
						self.value_tables[target_tier].write_replace_plan(existing_address.offset(), key, val, log)?;
					} else {
						self.value_tables[existing_tier].write_remove_plan(existing_address.offset(), log)?;
						let new_offset = self.value_tables[target_tier].write_insert_plan(key, val, log)?;
						let new_address = Address::new(new_offset, target_tier as u8);
						self.index.write_insert_plan(key, new_address, log, true)?;
					}
				} else {
					self.trigger_rebalance();
					return self.write_plan(key, value, log);
				}
			} else {
				let offset = self.value_tables[target_tier].write_insert_plan(key, val, log)?;
				let address = Address::new(offset, target_tier as u8);
				match self.index.write_insert_plan(key, address, log, true)? {
					PlanOutcome::NeedRebalance => {
						self.trigger_rebalance();
						return self.write_plan(key, value, log);
					}
					_ => {}
				}
			}
		} else {
			// Deletion
			let existing_entry = self.index.get_planned(key, log);
			if !existing_entry.is_empty() {
				let existing_tier = existing_entry.address().size_tier() as usize;
				// TODO: Remove this check? Highly unlikely.
				if self.value_tables[existing_tier].has_key_at(existing_entry.address().offset(), &key, log)? {
					self.value_tables[existing_tier].write_remove_plan(existing_entry.address().offset(), log)?;
					self.index.write_remove_plan(key, log)?;
				}
			}
			self.blobs.remove(key);
		}
		Ok(())
	}

	pub fn enact_plan(&mut self, action: LogAction, log: &mut LogReader) -> Result<()> {
		match action {
			LogAction::InsertIndex(record) => {
				if self.index.id == record.table {
					self.index.enact_plan(record.index, log)?;
				}
				if let Some(table) = self.rebalancing.iter_mut().find(|r|r.id == record.table) {
					table.enact_plan(record.index, log)?;
				}
				else {
					log::warn!(
						target: "parity-db",
						"Missing table {}",
						record.table,
					);
					return Err(Error::Corruption("Missing table".into()));
				}
			},
			LogAction::InsertValue(record) => {
				self.value_tables[record.table.size_tier() as usize].enact_plan(record.index, log)?;
			}
			_ => panic!("Unexpected log action"),
		}
		Ok(())
	}

	pub fn complete_plan(&mut self) -> Result<()> {
		for t in self.value_tables.iter_mut() {
			t.complete_plan()?;
		}
		Ok(())
	}

	pub fn rebalance(&mut self, log: &mut Log) -> Result<RebalanceProgress> {
		if let Some(source) = self.rebalancing.front_mut() {
			if self.rebalance_progress != source.id.total_chunks() {
				let mut writer = log.begin_record();
				log::trace!(target: "parity-db", "{}: Start rebalance record {}", self.index.id, writer.record_id());
				let mut source_index = self.rebalance_progress;
				let mut count = 0;
				log::trace!(target: "parity-db", "{}: Continue rebalance at {}", self.index.id, source_index);
				while source_index < source.id.total_chunks() && count < MAX_REBALANCE_BATCH {
					for entry in source.planned_entries(source_index, &mut writer).iter() {
						if entry.is_empty() {
							continue;
						}
						let mut key = self.value_tables[entry.address().size_tier() as usize]
							.partial_key_at(entry.address().offset(), &mut writer)?;

						// restore 16 high bits
						&mut key[0..2].copy_from_slice(&((source_index & 0xffff) as u16).to_be_bytes());
						match self.index.write_insert_plan(&key, entry.address(), &mut writer, false)? {
							PlanOutcome::NeedRebalance => panic!("Table requires double rebalance"),
							_ => {},
						}
						count += 1;
					}
					source_index += 1;
				}
				log::trace!(target: "parity-db", "{}: End rebalance batch {} ({})", self.index.id, source_index, count);
				self.rebalance_progress = source_index;

				if self.rebalance_progress == source.id.total_chunks() {
					log::info!(target: "parity-db", "Completed rebalance {}", self.index.id);
					writer.drop_table(source.id);
					return Ok(RebalanceProgress::Inactive)
				}
				log::trace!(target: "parity-db", "{}: End rebalance record {}", self.index.id, writer.record_id());
				let l = writer.drain();
				log.end_record(l)?;
				return Ok(RebalanceProgress::InProgress((self.rebalance_progress, source.id.total_chunks())))
			}
		}
		Ok(RebalanceProgress::Inactive)
	}

	pub fn drop_index(&mut self, id: IndexTableId) -> Result<()> {
		log::debug!(target: "parity-db", "Dropping {}", id);
		if self.rebalancing.front_mut().map_or(false, |index| index.id == id) {
			let table = self.rebalancing.pop_front();
			self.rebalance_progress = 0;
			table.unwrap().drop_file()?;
		} else {
			return Err(Error::Corruption("Dropping invalid index".into()));
		}
		Ok(())
	}
}

