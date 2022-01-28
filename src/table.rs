// Copyright 2015-2021 Parity Technologies (UK) Ltd.
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

// On disk data layout for value tables.
//
// Entry 0 (metadata)
// [LAST_REMOVED: 8][FILLED: 8]
// LAST_REMOVED - 64-bit index of removed entries linked list head
// FILLED - highest index filled with live data
//
// Complete entry:
// [SIZE: 2][REFS: 4][KEY: 26][VALUE]
// SIZE: 15-bit value size. Sizes up to 0x7ffc are allowed.
// This includes size of REFS and KEY.
// The first bit is reserved to indicate if compression is applied.
// REF: 32-bit reference counter (optional).
// KEY: lower 26 bytes of the key.
// VALUE: payload bytes.
//
// Partial entry (first part):
// [MULTIHEAD: 2][NEXT: 8][REFS: 4][KEY: 26][VALUE]
// MULTIHEAD - Split entry head marker. 0xfffd.
// NEXT - 64-bit index of the entry that holds the next part.
// take all available space in this entry.
// REF: 32-bit reference counter (optional).
// KEY: lower 26 bytes of the key. Under different condition
// can be skipped.
// VALUE: The rest of the entry is filled with payload bytes.
//
// Partial entry (continuation):
// [MULTIPART: 2][NEXT: 8][VALUE]
// MULTIPART - Split entry marker. 0xfffe.
// NEXT - 64-bit index of the entry that holds the next part.
// VALUE: The rest of the entry is filled with payload bytes.
//
// Partial entry (last part):
// [SIZE: 2][VALUE: SIZE]
// SIZE: 15-bit size of the remaining payload, also indicate
// if value is compressed.
// VALUE: SIZE payload bytes.
//
// Deleted entry
// [TOMBSTONE: 2][NEXT: 8]
// TOMBSTONE - Deleted entry marker. 0xffff
// NEXT - 64-bit index of the next deleted entry.


use std::convert::TryInto;
use std::mem::MaybeUninit;
use std::io::Read;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use crate::{
	table::key::{TableKey, TableKeyQuery, PARTIAL_SIZE},
	error::Result,
	column::ColId,
	log::{LogQuery, LogReader, LogWriter},
	display::hex,
	options::ColumnOptions as Options,
};

pub const SIZE_TIERS: usize = 1usize << SIZE_TIERS_BITS;
pub const SIZE_TIERS_BITS: u8 = 8;
pub const COMPRESSED_MASK: u16 = 0x80_00;
pub const MAX_ENTRY_SIZE: usize = 0x7ff8; // Actual max size in V4 was 0x7dfe
pub const MIN_ENTRY_SIZE: usize = 32;
const REFS_SIZE: usize = 4;
const SIZE_SIZE: usize = 2;
const INDEX_SIZE: usize = 8;
const MAX_ENTRY_BUF_SIZE: usize = 0x8000;

const TOMBSTONE: &[u8] = &[0xff, 0xff];
const MULTIPART_V4: &[u8] = &[0xff, 0xfe];
const MULTIHEAD_V4: &[u8] = &[0xff, 0xfd];
const MULTIPART: &[u8] = &[0xfe, 0xff];
const MULTIHEAD: &[u8] = &[0xfd, 0xff];
// When a rc reach locked ref, it is locked in db.
const LOCKED_REF: u32 = u32::MAX;


pub type Value = Vec<u8>;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct TableId(u16);

impl TableId {
	pub fn new(col: ColId, size_tier: u8) -> TableId {
		TableId(((col as u16) << 8) | size_tier as u16)
	}

	pub fn from_u16(id: u16) -> TableId {
		TableId(id)
	}

	pub fn col(&self) -> ColId {
		(self.0 >> 8) as ColId
	}

	pub fn size_tier(&self) -> u8 {
		(self.0 & 0xff) as u8
	}

	pub fn file_name(&self) -> String {
		format!("table_{:02}_{}", self.col(), hex(&[self.size_tier()]))
	}

	pub fn is_file_name(col: ColId, name: &str) -> bool {
		name.starts_with(&format!("table_{:02}_", col))
	}

	pub fn as_u16(&self) -> u16 {
		self.0
	}
}

impl std::fmt::Display for TableId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{:02}-{:02}", self.col(), hex(&[self.size_tier()]))
	}
}

pub struct ValueTable {
	pub id: TableId,
	pub entry_size: u16,
	file: crate::file::TableFile,
	filled: AtomicU64,
	last_removed: AtomicU64,
	dirty_header: AtomicBool,
	multipart: bool,
	ref_counted: bool,
	db_version: u32,
}

#[derive(Default, Clone, Copy)]
struct Header([u8; 16]);

impl Header {
	fn last_removed(&self) -> u64 {
		u64::from_le_bytes(self.0[0..INDEX_SIZE].try_into().unwrap())
	}
	fn set_last_removed(&mut self, last_removed: u64) {
		self.0[0..INDEX_SIZE].copy_from_slice(&last_removed.to_le_bytes());
	}
	fn filled(&self) -> u64 {
		u64::from_le_bytes(self.0[INDEX_SIZE..INDEX_SIZE * 2].try_into().unwrap())
	}
	fn set_filled(&mut self, filled: u64) {
		self.0[INDEX_SIZE..INDEX_SIZE * 2].copy_from_slice(&filled.to_le_bytes());
	}
}

pub(crate) struct Entry<B: AsRef<[u8]> + AsMut<[u8]>>(usize, B);
type FullEntry = Entry<[u8; MAX_ENTRY_BUF_SIZE]>;
type PartialEntry = Entry<[u8; 10]>;
type PartialKeyEntry = Entry<[u8; 40]>; // 2 + 4 + 26 + 8

impl<B: AsRef<[u8]> + AsMut<[u8]>> Entry<B> {
	#[inline(always)]
	pub(crate) fn new_uninit() -> Self {
		Entry(0, unsafe { MaybeUninit::uninit().assume_init() })
	}

	#[inline(always)]
	pub(crate) fn new(data: B) -> Self {
		Entry(0, data)
	}

	pub(crate) fn set_offset(&mut self, offset: usize) {
		self.0 = offset;
	}

	pub(crate) fn offset(&self) -> usize {
		self.0
	}

	pub(crate) fn write_slice(&mut self, buf: &[u8]) {
		let start = self.0;
		self.0 += buf.len();
		self.1.as_mut()[start..self.0].copy_from_slice(buf);
	}

	pub(crate) fn read_slice(&mut self, size: usize) -> &[u8] {
		let start = self.0;
		self.0 += size;
		&self.1.as_ref()[start..self.0]
	}

	fn is_tombstone(&self) -> bool {
		&self.1.as_ref()[0..SIZE_SIZE] == TOMBSTONE
	}

	fn write_tombstone(&mut self) {
		self.write_slice(&TOMBSTONE);
	}

	fn is_multipart(&self) -> bool {
		&self.1.as_ref()[0..SIZE_SIZE] == MULTIPART
	}

	fn is_multipart_v4(&self) -> bool {
		&self.1.as_ref()[0..SIZE_SIZE] == MULTIPART_V4
	}

	fn write_multipart(&mut self) {
		self.write_slice(&MULTIPART);
	}

	fn is_multihead(&self) -> bool {
		&self.1.as_ref()[0..SIZE_SIZE] == MULTIHEAD
	}

	fn is_multihead_v4(&self) -> bool {
		&self.1.as_ref()[0..SIZE_SIZE] == MULTIHEAD_V4
	}

	fn write_multihead(&mut self) {
		self.write_slice(&MULTIHEAD);
	}

	fn is_multi(&self, db_version: u32) -> bool {
		self.is_multipart() || self.is_multihead() ||
			(db_version <= 4 && (self.is_multipart_v4() || self.is_multihead_v4()))
	}

	fn read_size(&mut self) -> (u16, bool) {
		let size = u16::from_le_bytes(self.read_slice(SIZE_SIZE).try_into().unwrap());
		let compressed = (size & COMPRESSED_MASK) > 0;
		(size & !COMPRESSED_MASK, compressed)
	}

	fn skip_size(&mut self) {
		self.0 += SIZE_SIZE;
	}

	fn write_size(&mut self, mut size: u16, compressed: bool) {
		if compressed {
			size |= COMPRESSED_MASK;
		}
		self.write_slice(&size.to_le_bytes());
	}

	pub(crate) fn read_u64(&mut self) -> u64 {
		u64::from_le_bytes(self.read_slice(8).try_into().unwrap())
	}

	fn read_next(&mut self) -> u64 {
		self.read_u64()
	}

	pub(crate) fn skip_u64(&mut self) {
		self.0 += 8;
	}

	pub(crate) fn skip_next(&mut self) {
		self.skip_u64()
	}

	pub(crate) fn write_u64(&mut self, next_index: u64) {
		self.write_slice(&next_index.to_le_bytes());
	}

	fn write_next(&mut self, next_index: u64) {
		self.write_u64(next_index)
	}

	pub(crate) fn read_u32(&mut self) -> u32 {
		u32::from_le_bytes(self.read_slice(REFS_SIZE).try_into().unwrap())
	}

	pub(crate) fn write_u32(&mut self, next_index: u32) {
		self.write_slice(&next_index.to_le_bytes());
	}

	fn read_rc(&mut self) -> u32 {
		self.read_u32()
	}

	fn write_rc(&mut self, rc: u32) {
		self.write_slice(&rc.to_le_bytes());
	}

	fn read_partial(&mut self) -> &[u8] {
		self.read_slice(PARTIAL_SIZE)
	}

	fn remaining_to(&self, end: usize) -> &[u8] {
		&self.1.as_ref()[self.0..end]
	}

	pub(crate) fn inner_mut(&mut self) -> &mut B {
		&mut self.1
	}
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> AsMut<[u8]> for Entry<B> {
	fn as_mut(&mut self) -> &mut [u8] {
		self.1.as_mut()
	}
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> AsRef<[u8]> for Entry<B> {
	fn as_ref(&self) -> &[u8] {
		self.1.as_ref()
	}
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> std::ops::Index<std::ops::Range<usize>> for Entry<B> {
	type Output = [u8];

	fn index(&self, index: std::ops::Range<usize>) -> &[u8] {
		&self.1.as_ref()[index]
	}
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> std::ops::IndexMut<std::ops::Range<usize>> for Entry<B> {
	fn index_mut(&mut self, index: std::ops::Range<usize>) -> &mut [u8] {
		&mut self.1.as_mut()[index]
	}
}

impl ValueTable {
	pub fn open(
		path: Arc<std::path::PathBuf>,
		id: TableId,
		entry_size: Option<u16>,
		options: &Options,
		db_version: u32,
	) -> Result<ValueTable> {
		let (multipart, entry_size) = match entry_size {
			Some(s) => (false, s),
			None => (true, 4096),
		};
		assert!(entry_size >= MIN_ENTRY_SIZE as u16);
		if db_version >= 4 {
			assert!(entry_size <= MAX_ENTRY_SIZE as u16);
		}

		let mut filepath: std::path::PathBuf = std::path::PathBuf::clone(&*path);
		filepath.push(id.file_name());
		let file = crate::file::TableFile::open(filepath, entry_size, id)?;
		let mut filled = 1;
		let mut last_removed = 0;
		if let Some(file) = &mut *file.file.write() {
			let mut header = Header::default();
			file.read_exact(&mut header.0)?;
			last_removed = header.last_removed();
			filled = header.filled();
			if filled == 0 {
				filled = 1;
			}
			log::debug!(target: "parity-db", "Opened value table {} with {} entries, entry_size={}", id, filled, entry_size);
		}

		Ok(ValueTable {
			id,
			entry_size,
			file,
			filled: AtomicU64::new(filled),
			last_removed: AtomicU64::new(last_removed),
			dirty_header: AtomicBool::new(false),
			multipart,
			ref_counted: options.ref_counted,
			db_version,
		})
	}

	pub(crate) fn value_size(&self, key: &TableKey) -> Option<u16> {
		let base = self.entry_size - SIZE_SIZE as u16 - self.ref_size() as u16;
		let k_encoded = key.encoded_size() as u16;
		if base < k_encoded {
			return None;
		} else {
			Some(base - k_encoded)
		}
	}

	// Return ref counter, partial key and if it was compressed.
	#[inline(always)]
	fn for_parts(
		&self,
		key: &mut TableKeyQuery,
		mut index: u64,
		log: &impl LogQuery,
		mut f: impl FnMut(&[u8]) -> bool,
	) -> Result<(u32, bool)> {
		let mut buf = FullEntry::new_uninit();
		let mut part = 0;
		let mut compressed = false;
		let mut rc = 1;
		let entry_size = self.entry_size as usize;
		loop {
			let buf = if log.value(self.id, index, buf.as_mut()) {
				&mut buf
			} else {
				log::trace!(
					target: "parity-db",
					"{}: Query slot {}",
					self.id,
					index,
				);
				self.file.read_at(&mut buf[0..entry_size], index * self.entry_size as u64)?;
				&mut buf
			};

			buf.set_offset(0);

			if buf.is_tombstone() {
				return Ok((0, false));
			}

			let (entry_end, next) = if self.multipart && buf.is_multi(self.db_version) {
				buf.skip_size();
				let next = buf.read_next();
				(entry_size, next)
			} else {
				let (size, read_compressed) = buf.read_size();
				compressed = read_compressed;
				(buf.offset() + size as usize, 0)
			};

			if part == 0 {
				if self.ref_counted {
					rc = buf.read_rc();
				}
				match key {
					TableKeyQuery::Fetch(Some(to_fetch)) => {
						**to_fetch = TableKey::fetch_partial(buf)?;
					},
					TableKeyQuery::Fetch(None) => (),
					TableKeyQuery::Check(k) => {
						let to_fetch = k.fetch(buf)?;
						if !k.compare(&to_fetch) {
							log::debug!(
								target: "parity-db",
								"{}: Key mismatch at {}. Expected {}, got {:?}, size = {}",
								self.id,
								index,
								k,
								to_fetch,
								self.entry_size,
							);
							return Ok((0, false));
						}
					},
				}
			}
			if !f(buf.remaining_to(entry_end)) {
				break;
			};

			if next == 0 {
				break;
			}
			part += 1;
			index = next;
		}
		Ok((rc, compressed))
	}

	pub(crate) fn get(&self, key: &TableKey, index: u64, log: &impl LogQuery) -> Result<Option<(Value, bool)>> {
		if let Some((value, compressed, _)) = self.query(&mut TableKeyQuery::Check(key), index, log)? {
			Ok(Some((value, compressed)))
		} else {
			Ok(None)
		}
	}

	pub(crate) fn query(&self, key: &mut TableKeyQuery, index: u64, log: &impl LogQuery) -> Result<Option<(Value, bool, u32)>> {
		let mut result = Vec::new();
		let (rc, compressed) = self.for_parts(key, index, log, |buf| {
			result.extend_from_slice(buf);
			true
		})?;
		if rc > 0 {
			return Ok(Some((result, compressed, rc)));
		}
		Ok(None)
	}

	pub fn get_with_meta(&self, index: u64, log: &impl LogQuery) -> Result<Option<(Value, u32, [u8; PARTIAL_SIZE], bool)>> {
		let mut query_key = Default::default();
		if let Some((value, compressed, rc)) = self.query(&mut TableKeyQuery::Fetch(Some(&mut query_key)), index, log)? {
			return Ok(Some((value, rc, query_key, compressed)));
		}
		Ok(None)
	}

	pub(crate) fn size(&self, key: &TableKey, index: u64, log: &impl LogQuery) -> Result<Option<(u32, bool)>> {
		let mut result = 0;
		let (rc, compressed) = self.for_parts(&mut TableKeyQuery::Check(key), index, log, |buf| {
			result += buf.len() as u32;
			true
		})?;
		if rc > 0 {
			return Ok(Some((result, compressed)));
		}
		Ok(None)
	}

	pub fn has_key_at(&self, index: u64, key: &TableKey, log: &LogWriter) -> Result<bool> {
		match key {
			TableKey::Partial(k) => Ok(match self.partial_key_at(index, log)? {
				Some(existing_key) => &existing_key[..] == key::partial_key(k),
				None => false,
			}),
			TableKey::NoHash => Ok(!self.is_tombstone(index, log)?),
		}
	}

	pub fn partial_key_at(&self, index: u64, log: &impl LogQuery) -> Result<Option<[u8; PARTIAL_SIZE]>> {
		let mut query_key = Default::default();
		let (rc, _compressed) = self.for_parts(&mut TableKeyQuery::Fetch(Some(&mut query_key)), index, log, |_buf| false)?;
		Ok(if rc == 0 {
			None
		} else {
			Some(query_key)
		})
	}

	pub fn is_tombstone(&self, index: u64, log: &impl LogQuery) -> Result<bool> {
		let mut buf = PartialKeyEntry::new_uninit();
		let buf = if log.value(self.id, index, buf.as_mut()) {
			&mut buf
		} else {
			self.file.read_at(buf.as_mut(), index * self.entry_size as u64)?;
			&mut buf
		};
		Ok(buf.is_tombstone())
	}

	pub fn read_next_free(&self, index: u64, log: &LogWriter) -> Result<u64> {
		let mut buf = PartialEntry::new_uninit();
		if !log.value(self.id, index, buf.as_mut()) {
			self.file.read_at(buf.as_mut(), index * self.entry_size as u64)?;
		}
		buf.skip_size();
		let next = buf.read_next();
		return Ok(next);
	}

	pub fn read_next_part(&self, index: u64, log: &LogWriter) -> Result<Option<u64>> {
		let mut buf = PartialEntry::new_uninit();
		if !log.value(self.id, index, buf.as_mut()) {
			self.file.read_at(buf.as_mut(), index * self.entry_size as u64)?;
		}
		if self.multipart && buf.is_multi(self.db_version) {
			buf.skip_size();
			let next = buf.read_next();
			return Ok(Some(next));
		}
		return Ok(None);
	}

	pub fn next_free(&self, log: &mut LogWriter) -> Result<u64> {
		let filled = self.filled.load(Ordering::Relaxed);
		let last_removed = self.last_removed.load(Ordering::Relaxed);
		let index = if last_removed != 0 {
			let next_removed = self.read_next_free(last_removed, log)?;
			log::trace!(
				target: "parity-db",
				"{}: Inserting into removed slot {}",
				self.id,
				last_removed,
			);
			self.last_removed.store(next_removed, Ordering::Relaxed);
			last_removed
		} else {
			log::trace!(
				target: "parity-db",
				"{}: Inserting into new slot {}",
				self.id,
				filled,
			);
			self.filled.store(filled + 1, Ordering::Relaxed);
			filled
		};
		self.dirty_header.store(true, Ordering::Relaxed);
		Ok(index)
	}

	fn overwrite_chain(&self, key: &TableKey, value: &[u8], log: &mut LogWriter, at: Option<u64>, compressed: bool) -> Result<u64> {
		let mut remainder = value.len() + self.ref_size() + key.encoded_size();
		let mut offset = 0;
		let mut start = 0;
		assert!(self.multipart || value.len() <= self.value_size(key).unwrap() as usize);
		let (mut index, mut follow) = match at {
			Some(index) => (index, true),
			None => (self.next_free(log)?, false)
		};
		loop {
			let mut next_index = 0;
			if follow {
				// check existing link
				match self.read_next_part(index, log)? {
					Some(next) => {
						next_index = next;
					}
					None => {
						follow = false;
					}
				}
			}
			log::trace!(
				target: "parity-db",
				"{}: Writing slot {}: {}",
				self.id,
				index,
				key,
			);
			let mut buf = FullEntry::new_uninit();
			let free_space = self.entry_size as usize - SIZE_SIZE;
			let value_len = if remainder > free_space {
				if !follow {
					next_index = self.next_free(log)?
				}
				if start == 0 {
					buf.write_multihead();
				} else {
					buf.write_multipart();
				}
				buf.write_next(next_index);
				free_space - INDEX_SIZE
			} else {
				buf.write_size(remainder as u16, compressed);
				remainder
			};
			let init_offset = buf.offset();
			if offset == 0 {
				if self.ref_counted {
					// first rc.
					buf.write_rc(1u32);
				}
				key.write(&mut buf);
			}
			let written = buf.offset() - init_offset;
			buf.write_slice(&value[offset..offset + value_len - written]);
			offset += value_len - written;
			log.insert_value(self.id, index, buf[0..buf.offset()].to_vec());
			remainder -= value_len;
			if start == 0 {
				start = index;
			}
			index = next_index;
			if remainder == 0 {
				if index != 0 {
					// End of new entry. Clear the remaining tail and exit
					self.clear_chain(index, log)?;
				}
				break;
			}
		}

		Ok(start)
	}

	fn clear_chain(&self, mut index: u64, log: &mut LogWriter) -> Result<()> {
		loop {
			match self.read_next_part(index, log)? {
				Some(next) => {
					self.clear_slot(index, log)?;
					index = next;
				}
				None => {
					self.clear_slot(index, log)?;
					return Ok(());
				}
			}
		}
	}

	fn clear_slot(&self, index: u64, log: &mut LogWriter) -> Result<()> {
		let last_removed = self.last_removed.load(Ordering::Relaxed);
		log::trace!(
			target: "parity-db",
			"{}: Freeing slot {}",
			self.id,
			index,
		);

		let mut buf = PartialEntry::new_uninit();
		buf.write_tombstone();
		buf.write_next(last_removed);

		log.insert_value(self.id, index, buf[0..buf.offset()].to_vec());
		self.last_removed.store(index, Ordering::Relaxed);
		self.dirty_header.store(true, Ordering::Relaxed);
		Ok(())
	}

	pub(crate) fn write_insert_plan(&self, key: &TableKey, value: &[u8], log: &mut LogWriter, compressed: bool) -> Result<u64> {
		self.overwrite_chain(key, value, log, None, compressed)
	}

	pub(crate) fn write_replace_plan(&self, index: u64, key: &TableKey, value: &[u8], log: &mut LogWriter, compressed: bool) -> Result<()> {
		self.overwrite_chain(key, value, log, Some(index), compressed)?;
		Ok(())
	}

	pub fn write_remove_plan(&self, index: u64, log: &mut LogWriter) -> Result<()> {
		if self.multipart {
			self.clear_chain(index, log)?;
		} else {
			self.clear_slot(index, log)?;
		}
		Ok(())
	}

	pub fn write_inc_ref(&self, index: u64, log: &mut LogWriter) -> Result<()> {
		self.change_ref(index, 1, log)?;
		Ok(())
	}

	pub fn write_dec_ref(&self, index: u64, log: &mut LogWriter) -> Result<bool> {
		if self.change_ref(index, -1, log)? {
			return Ok(true);
		}
		self.write_remove_plan(index, log)?;
		Ok(false)
	}

	pub fn change_ref(&self, index: u64, delta: i32, log: &mut LogWriter) -> Result<bool> {
		let mut buf = FullEntry::new_uninit();
		let buf = if log.value(self.id, index, buf.as_mut()) {
			&mut buf
		} else {
			self.file.read_at(&mut buf[0..self.entry_size as usize], index * self.entry_size as u64)?;
			&mut buf
		};

		if buf.is_tombstone() {
			return Ok(false);
		}

		let size = if self.multipart && buf.is_multi(self.db_version) {
			buf.skip_size();
			buf.skip_next();
			self.entry_size as usize
		} else {
			let (size, _compressed) = buf.read_size();
			buf.offset() + size as usize
		};

		let rc_offset = buf.offset();
		let mut counter = buf.read_rc();
		if delta > 0 {
			if counter >= LOCKED_REF - delta as u32 {
				counter = LOCKED_REF
			} else {
				counter = counter + delta as u32;
			}
		} else {
			if counter != LOCKED_REF {
				counter = counter.saturating_sub(-delta as u32);
				if counter == 0 {
					return Ok(false);
				}
			}
		}

		buf.set_offset(rc_offset);
		buf.write_rc(counter);
		// TODO: optimize actual buf size
		log.insert_value(self.id, index, buf[0..size].to_vec());
		return Ok(true);
	}

	pub fn enact_plan(&self, index: u64, log: &mut LogReader) -> Result<()> {
		while index >= self.file.capacity.load(Ordering::Relaxed) {
			self.file.grow(self.entry_size)?;
		}
		if index == 0 {
			let mut header = Header::default();
			log.read(&mut header.0)?;
			self.file.write_at(&header.0, 0)?;
			return Ok(());
		}

		let mut buf = FullEntry::new_uninit();
		log.read(&mut buf[0..SIZE_SIZE])?;
		if buf.is_tombstone() {
			log.read(&mut buf[SIZE_SIZE..SIZE_SIZE + INDEX_SIZE])?;
			self.file.write_at(&buf[0..SIZE_SIZE + INDEX_SIZE], index * (self.entry_size as u64))?;
			log::trace!(target: "parity-db", "{}: Enacted tombstone in slot {}", self.id, index);
		} else if self.multipart && buf.is_multi(self.db_version) {
				let entry_size = self.entry_size as usize;
				log.read(&mut buf[SIZE_SIZE..entry_size])?;
				self.file.write_at(&buf[0..entry_size], index * (entry_size as u64))?;
				log::trace!(target: "parity-db", "{}: Enacted multipart in slot {}", self.id, index);
		} else {
			let (len, _compressed) = buf.read_size();
			log.read(&mut buf[SIZE_SIZE..SIZE_SIZE + len as usize])?;
			self.file.write_at(&buf[0..(SIZE_SIZE + len as usize)], index * (self.entry_size as u64))?;
			log::trace!(target: "parity-db", "{}: Enacted {}: {}, {} bytes", self.id, index, hex(&buf.1[6..32]), len);
		}
		Ok(())
	}

	pub fn validate_plan(&self, index: u64, log: &mut LogReader) -> Result<()> {
		if index == 0 {
			let mut header = Header::default();
			log.read(&mut header.0)?;
			// TODO: sanity check last_removed and filled
			return Ok(());
		}
		let mut buf = FullEntry::new_uninit();
		log.read(&mut buf[0..SIZE_SIZE])?;
		if buf.is_tombstone() {
			log.read(&mut buf[SIZE_SIZE..SIZE_SIZE + INDEX_SIZE])?;
			log::trace!(target: "parity-db", "{}: Validated tombstone in slot {}", self.id, index);
		} else if self.multipart && buf.is_multi(self.db_version) {
			let entry_size = self.entry_size as usize;
			log.read(&mut buf[SIZE_SIZE..entry_size])?;
			log::trace!(target: "parity-db", "{}: Validated multipart in slot {}", self.id, index);
		} else {
			// TODO: check len
			let (len, _compressed) = buf.read_size();
			log.read(&mut buf[SIZE_SIZE..SIZE_SIZE + len as usize])?;
			log::trace!(target: "parity-db", "{}: Validated {}: {}, {} bytes", self.id, index, hex(&buf[SIZE_SIZE..32]), len);
		}
		Ok(())
	}

	pub fn refresh_metadata(&self) -> Result<()> {
		if self.file.file.read().is_none() {
			return Ok(());
		}
		let mut header = Header::default();
		self.file.read_at(&mut header.0, 0)?;
		let last_removed = header.last_removed();
		let mut filled = header.filled();
		if filled == 0 {
			filled = 1;
		}
		self.last_removed.store(last_removed, Ordering::Relaxed);
		self.filled.store(filled, Ordering::Relaxed);
		Ok(())
	}

	pub fn complete_plan(&self, log: &mut LogWriter) -> Result<()> {
		if let Ok(true) = self.dirty_header.compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed) {
			// last_removed or filled pointers were modified. Add them to the log
			let mut buf = Header::default();
			let last_removed = self.last_removed.load(Ordering::Relaxed);
			let filled = self.filled.load(Ordering::Relaxed);
			buf.set_last_removed(last_removed);
			buf.set_filled(filled);
			log.insert_value(self.id, 0, buf.0.to_vec());
		}
		Ok(())
	}

	pub fn flush(&self) -> Result<()> {
		self.file.flush()
	}

	fn ref_size(&self) -> usize {
		if self.ref_counted {
			REFS_SIZE
		} else {
			0
		}
	}

	pub fn iter_while(&self, log: &impl LogQuery, mut f: impl FnMut (u64, u32, Vec<u8>, bool) -> bool) -> Result<()> {
		let filled = self.filled.load(Ordering::Relaxed);
		for index in 1 .. filled {
			let mut result = Vec::new();
			// expect only indexed key.
			let mut _fetch_key = Default::default();
			match self.for_parts(&mut TableKeyQuery::Fetch(Some(&mut _fetch_key)), index, log, |buf| {
				result.extend_from_slice(buf);
				true
			}) {
				Ok((rc, compressed)) => if rc > 0 {
					if !f(index, rc, result, compressed) {
						break;
					}
				}
				Err(crate::error::Error::InvalidValueData) => (), // ignore, can be external index.
				Err(e) => return Err(e),
			}
		}
		Ok(())
	}

	pub fn is_init(&self) -> bool {
		self.file.file.read().is_some()
	}

	pub fn init_with_entry(&self, entry: &[u8]) -> Result<()> {
		self.file.grow(self.entry_size)?;

		let empty_overlays = parking_lot::RwLock::new(Default::default());
		let mut log = LogWriter::new(&empty_overlays, 0);
		let at = self.overwrite_chain(&TableKey::NoHash, entry, &mut log, None, false)?;
		self.complete_plan(&mut log)?;
		assert!(at == 1);
		let log = log.drain();
		let change = log.local_values_changes(self.id).expect("entry written above");
		for (at, (_rec_id, entry)) in change.map.iter() {
			self.file.write_at(entry.as_slice(), *at * (self.entry_size as u64))?;
		}
		Ok(())
	}
}

pub mod key {
	use super::FullEntry;
	use crate::{Result, Key};

	pub const PARTIAL_SIZE: usize = 26;

	pub fn partial_key(hash: &Key) -> &[u8] {
		&hash[6..]
	}

	pub enum TableKey {
		Partial(Key),
		NoHash,
	}

	impl TableKey {
		pub fn encoded_size(&self) -> usize {
			match self {
				TableKey::Partial(_) => PARTIAL_SIZE,
				TableKey::NoHash => 0,
			}
		}

		pub fn index_from_partial(partial: &[u8]) -> u64 {
			u64::from_be_bytes((partial[0..8]).try_into().unwrap())
		}

		pub fn index(&self) -> Option<u64> {
			match self {
				TableKey::Partial(k) => {
					Some(Self::index_from_partial(k))
				},
				TableKey::NoHash => {
					None
				},
			}
		}

		pub fn compare(&self, fetch: &Option<[u8; PARTIAL_SIZE]>) -> bool {
			match (self, fetch) {
				(TableKey::Partial(k), Some(fetch)) => {
					partial_key(k) == fetch
				},
				(TableKey::NoHash, _) => true,
				_ => false,
			}
		}

		pub(crate) fn fetch_partial(buf: &mut super::FullEntry)-> Result<[u8; PARTIAL_SIZE]> {
			let mut result = [0u8; PARTIAL_SIZE];
			if buf.1.len() >= PARTIAL_SIZE {
				let pks = buf.read_partial();
				result.copy_from_slice(&pks);
				return Ok(result)
			}
			Err(crate::error::Error::InvalidValueData)
		}

		pub(crate) fn fetch(&self, buf: &mut super::FullEntry)-> Result<Option<[u8; PARTIAL_SIZE]>> {
			match self {
				TableKey::Partial(_k) => Ok(Some(Self::fetch_partial(buf)?)),
				TableKey::NoHash => Ok(None),
			}
		}

		pub(crate) fn write(&self, buf: &mut FullEntry) {
			match self {
				TableKey::Partial(k) => {
					buf.write_slice(partial_key(k));
				},
				TableKey::NoHash => (),
			}
		}
	}

	impl std::fmt::Display for TableKey {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			match self {
				TableKey::Partial(k) => write!(f, "{}", crate::display::hex(k)),
				TableKey::NoHash => write!(f, "no_hash"),
			}
		}
	}

	pub(crate) enum TableKeyQuery<'a> {
		Check(&'a TableKey),
		Fetch(Option<&'a mut [u8; PARTIAL_SIZE]>),
	}
}

#[cfg(test)]
mod test {
	const ENTRY_SIZE: u16 = 64;
	use crate::Key;
	use crate::table::key::TableKey;
	use super::{ValueTable, TableId, Value};
	use crate::{log::{Log, LogWriter, LogAction}, options::{Options, ColumnOptions, CURRENT_VERSION}};

	struct TempDir(std::sync::Arc<std::path::PathBuf>);

	impl TempDir {
		fn new(name: &'static str) -> TempDir {
			env_logger::try_init().ok();
			let mut path = std::env::temp_dir();
			path.push("parity-db-test");
			path.push("value-table");
			path.push(name);

			if path.exists() {
				std::fs::remove_dir_all(&path).unwrap();
			}
			std::fs::create_dir_all(&path).unwrap();
			TempDir(std::sync::Arc::new(path))
		}

		fn table(&self, size: Option<u16>, options: &ColumnOptions) -> ValueTable {
			let id = TableId::new(0, 0);
			ValueTable::open(self.0.clone(), id, size, options, CURRENT_VERSION).unwrap()
		}

		fn log(&self) -> Log {
			let options = Options::with_columns(&*self.0, 1);
			Log::open(&options).unwrap()
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			if self.0.exists() {
				std::fs::remove_dir_all(&*self.0).unwrap();
			}
		}
	}

	fn write_ops<F: FnOnce(&mut LogWriter)>(table: &ValueTable, log: &Log, f: F) {
		let mut writer = log.begin_record();
		f(&mut writer);
		let bytes_written = log.end_record(writer.drain()).unwrap();
		// Cycle through 2 log files
		let _ = log.read_next(false);
		log.flush_one(0).unwrap();
		let _ = log.read_next(false);
		log.flush_one(0).unwrap();
		let mut reader = log.read_next(false).unwrap().unwrap();
		loop {
			match reader.next().unwrap() {
				LogAction::BeginRecord
					| LogAction::InsertIndex { .. }
					| LogAction::DropTable { .. } => {
					panic!("Unexpected log entry");
				},
				LogAction::EndRecord => {
					let bytes_read = reader.read_bytes();
					assert_eq!(bytes_written, bytes_read);
					break;
				},
				LogAction::InsertValue(insertion) => {
					table.enact_plan(insertion.index, &mut reader).unwrap();
				},
			}
		}
	}

	fn key(k: u32) -> Key {
		let mut key = Key::default();
		key.copy_from_slice(blake2_rfc::blake2b::blake2b(32, &[], &k.to_le_bytes()).as_bytes());
		key
	}

	fn simple_key(k: Key) -> TableKey {
		TableKey::Partial(k)
	}

	fn no_hash(_: Key) -> TableKey {
		TableKey::NoHash
	}

	fn value(size: usize) -> Value {
		use rand::RngCore;
		let mut result = Vec::with_capacity(size);
		result.resize(size, 0);
		rand::thread_rng().fill_bytes(&mut result);
		result
	}

	fn rc_options() -> ColumnOptions {
		let mut result = ColumnOptions::default();
		result.ref_counted = true;
		result
	}

	#[test]
	fn insert_simple() {
		insert_simple_inner(&Default::default());
		insert_simple_inner(&rc_options());
	}
	fn insert_simple_inner(options: &ColumnOptions) {
		let dir = TempDir::new("insert_simple");
		let table = dir.table(Some(ENTRY_SIZE), options);
		let log = dir.log();

		let key = key(1);
		let key = TableKey::Partial(key);
		let key = &key;
		let val = value(19);
		let compressed = true;

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key, &val, writer, compressed).unwrap();
			assert_eq!(table.get(key, 1, writer).unwrap(), Some((val.clone(), compressed)));
		});

		assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val, compressed)));
		assert_eq!(table.filled.load(std::sync::atomic::Ordering::Relaxed), 2);
	}

	#[test]
	#[should_panic(expected = "assertion failed: entry_size <= MAX_ENTRY_SIZE as u16")]
	fn oversized_into_fixed_panics() {
		let dir = TempDir::new("oversized_into_fixed_panics");
		let _table = dir.table(Some(65534), &Default::default());
	}

	#[test]
	fn remove_simple() {
		remove_simple_inner(&Default::default());
		remove_simple_inner(&rc_options());
	}
	fn remove_simple_inner(options: &ColumnOptions) {
		let dir = TempDir::new("remove_simple");
		let table = dir.table(Some(ENTRY_SIZE), options);
		let log = dir.log();

		let key1 = key(1);
		let key1 = &TableKey::Partial(key1);
		let key2 = key(2);
		let key2 = &TableKey::Partial(key2);
		let val1 = value(11);
		let val2 = value(21);
		let compressed = false;

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key1, &val1, writer, compressed).unwrap();
			table.write_insert_plan(key2, &val2, writer, compressed).unwrap();
		});

		write_ops(&table, &log, |writer| {
			table.write_remove_plan(1, writer).unwrap();
		});

		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), None);
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 1);

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key1, &val1, writer, compressed).unwrap();
		});
		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), Some((val1, compressed)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 0);
	}

	#[test]
	fn replace_simple() {
		replace_simple_inner(&Default::default(), simple_key);
		replace_simple_inner(&rc_options(), simple_key);
		replace_simple_inner(&Default::default(), no_hash);
		replace_simple_inner(&rc_options(), no_hash);
	}
	fn replace_simple_inner(options: &ColumnOptions, table_key: fn(Key) -> TableKey) {
		let dir = TempDir::new("replace_simple");
		let table = dir.table(Some(ENTRY_SIZE), options);
		let log = dir.log();

		let key1 = key(1);
		let key1 = &table_key(key1);
		let key2 = key(2);
		let key2 = &table_key(key2);
		let val1 = value(11);
		let val2 = value(21);
		let val3 = value(26); // max size for full hash and rc
		let compressed = true;

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key1, &val1, writer, compressed).unwrap();
			table.write_insert_plan(key2, &val2, writer, compressed).unwrap();
		});

		write_ops(&table, &log, |writer| {
			table.write_replace_plan(1, key2, &val3, writer, false).unwrap();
		});

		assert_eq!(table.get(key2, 1, log.overlays()).unwrap(), Some((val3, false)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 0);
	}

	#[test]
	fn replace_multipart_shorter() {
		replace_multipart_shorter_inner(&Default::default());
		replace_multipart_shorter_inner(&rc_options());
	}
	fn replace_multipart_shorter_inner(options: &ColumnOptions) {
		let dir = TempDir::new("replace_multipart_shorter");
		let table = dir.table(None, options);
		let log = dir.log();

		let key1 = key(1);
		let key1 = &TableKey::Partial(key1);
		let key2 = key(2);
		let key2 = &TableKey::Partial(key2);
		let val1 = value(20000);
		let val2 = value(30);
		let val1s = value(5000);
		let compressed = false;

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key1, &val1, writer, compressed).unwrap();
			table.write_insert_plan(key2, &val2, writer, compressed).unwrap();
		});

		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), Some((val1, compressed)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 0);
		assert_eq!(table.filled.load(std::sync::atomic::Ordering::Relaxed), 7);

		write_ops(&table, &log, |writer| {
			table.write_replace_plan(1, key1, &val1s, writer, compressed).unwrap();
		});
		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), Some((val1s, compressed)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 5);
		write_ops(&table, &log, |writer| {
			assert_eq!(table.read_next_free(5, writer).unwrap(), 4);
			assert_eq!(table.read_next_free(4, writer).unwrap(), 3);
			assert_eq!(table.read_next_free(3, writer).unwrap(), 0);
		});
	}

	#[test]
	fn replace_multipart_longer() {
		replace_multipart_longer_inner(&Default::default());
		replace_multipart_longer_inner(&rc_options());
	}
	fn replace_multipart_longer_inner(options: &ColumnOptions) {
		let dir = TempDir::new("replace_multipart_longer");
		let table = dir.table(None, options);
		let log = dir.log();

		let key1 = key(1);
		let key1 = &TableKey::Partial(key1);
		let key2 = key(2);
		let key2 = &TableKey::Partial(key2);
		let val1 = value(5000);
		let val2 = value(30);
		let val1l = value(20000);
		let compressed = false;

		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key1, &val1, writer, compressed).unwrap();
			table.write_insert_plan(key2, &val2, writer, compressed).unwrap();
		});

		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), Some((val1, compressed)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 0);
		assert_eq!(table.filled.load(std::sync::atomic::Ordering::Relaxed), 4);

		write_ops(&table, &log, |writer| {
			table.write_replace_plan(1, key1, &val1l, writer, compressed).unwrap();
		});
		assert_eq!(table.get(key1, 1, log.overlays()).unwrap(), Some((val1l, compressed)));
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 0);
		assert_eq!(table.filled.load(std::sync::atomic::Ordering::Relaxed), 7);
	}

	#[test]
	fn ref_counting() {
		for compressed in [false, true] {
			let dir = TempDir::new("ref_counting");
			let table = dir.table(None, &rc_options());
			let log = dir.log();

			let key = key(1);
			let key = &TableKey::Partial(key);
			let val = value(5000);

			write_ops(&table, &log, |writer| {
				table.write_insert_plan(key, &val, writer, compressed).unwrap();
				table.write_inc_ref(1, writer).unwrap();
			});
			assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val.clone(), compressed)));
			write_ops(&table, &log, |writer| {
				table.write_dec_ref(1, writer).unwrap();
			});
			assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val, compressed)));
			write_ops(&table, &log, |writer| {
				table.write_dec_ref(1, writer).unwrap();
			});
			assert_eq!(table.get(key, 1, log.overlays()).unwrap(), None);
		}
	}

	#[test]
	fn ref_underflow() {
		let dir = TempDir::new("ref_underflow");
		let table = dir.table(None, &rc_options());
		let log = dir.log();

		let key = key(1);
		let key = &TableKey::Partial(key);
		let val = value(10);

		let compressed = false;
		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key, &val, writer, compressed).unwrap();
			table.write_inc_ref(1, writer).unwrap();
		});
		assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val.clone(), compressed)));
		write_ops(&table, &log, |writer| {
			table.write_dec_ref(1, writer).unwrap();
			table.write_dec_ref(1, writer).unwrap();
			table.write_dec_ref(1, writer).unwrap();
		});
		assert_eq!(table.get(key, 1, log.overlays()).unwrap(), None);
	}

	#[test]
	fn multipart_collision() {
		let dir = TempDir::new("multipart_collision");
		let table = dir.table(Some(super::MAX_ENTRY_SIZE as u16), &rc_options());
		let log = dir.log();

		let key = key(1);
		let key = &TableKey::Partial(key);
		let val = value(32225); // This result in 0x7dff entry size, which conflicts with v4 multipart definition

		let compressed = true;
		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key, &val, writer, compressed).unwrap();
		});
		assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val.clone(), compressed)));
		write_ops(&table, &log, |writer| {
			table.write_dec_ref(1, writer).unwrap();
		});
		assert_eq!(table.last_removed.load(std::sync::atomic::Ordering::Relaxed), 1);

		// Check that max entry size values are OK.
		let value_size = table.value_size(key).unwrap();
		assert_eq!(0x7fd8, table.value_size(key).unwrap()); // Max value size for this configuration.
		let val = value(value_size as usize); // This result in 0x7ff8 entry size.
		write_ops(&table, &log, |writer| {
			table.write_insert_plan(key, &val, writer, compressed).unwrap();
		});
		assert_eq!(table.get(key, 1, log.overlays()).unwrap(), Some((val.clone(), compressed)));
	}
}
