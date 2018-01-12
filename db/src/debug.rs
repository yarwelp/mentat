// Copyright 2016 Mozilla
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use
// this file except in compliance with the License. You may obtain a copy of the
// License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

#![allow(dead_code)]

/// Low-level functions for testing.

use std::borrow::Borrow;
use std::io::{Write};
use std::rc::Rc;

use itertools::Itertools;
use rusqlite;
use rusqlite::types::{ToSql};
use tabwriter::TabWriter;

use bootstrap;
use db::TypedSQLValue;
use edn;
use entids;
use errors::Result;
use mentat_core::{
    HasSchema,
    SQLValueType,
    TypedValue,
    ValueType,
};
use mentat_tx::entities::{Entid};
use schema::{
    SchemaBuilding,
};
use types::Schema;

/// Represents a *datom* (assertion) in the store.
#[derive(Clone,Debug,Eq,Hash,Ord,PartialOrd,PartialEq)]
pub struct Datom {
    // TODO: generalize this.
    e: Entid,
    a: Entid,
    v: edn::Value,
    tx: i64,
    added: Option<bool>,
}

/// Represents a set of datoms (assertions) in the store.
///
/// To make comparision easier, we deterministically order.  The ordering is the ascending tuple
/// ordering determined by `(e, a, (value_type_tag, v), tx)`, where `value_type_tag` is an internal
/// value that is not exposed but is deterministic.
pub struct Datoms(pub Vec<Datom>);

/// Represents an ordered sequence of transactions in the store.
///
/// To make comparision easier, we deterministically order.  The ordering is the ascending tuple
/// ordering determined by `(e, a, (value_type_tag, v), tx, added)`, where `value_type_tag` is an
/// internal value that is not exposed but is deterministic, and `added` is ordered such that
/// retracted assertions appear before added assertions.
pub struct Transactions(pub Vec<Datoms>);

/// Represents the fulltext values in the store.
pub struct FulltextValues(pub Vec<(i64, String)>);

impl Datom {
    pub fn into_edn(&self) -> edn::Value {
        let f = |entid: &Entid| -> edn::Value {
            match *entid {
                Entid::Entid(ref y) => edn::Value::Integer(y.clone()),
                Entid::Ident(ref y) => edn::Value::NamespacedKeyword(y.clone()),
            }
        };

        let mut v = vec![f(&self.e), f(&self.a), self.v.clone()];
        if let Some(added) = self.added {
            v.push(edn::Value::Integer(self.tx));
            v.push(edn::Value::Boolean(added));
        }

        edn::Value::Vector(v)
    }
}

impl Datoms {
    pub fn into_edn(&self) -> edn::Value {
        edn::Value::Vector((&self.0).into_iter().map(|x| x.into_edn()).collect())
    }
}

impl Transactions {
    pub fn into_edn(&self) -> edn::Value {
        edn::Value::Vector((&self.0).into_iter().map(|x| x.into_edn()).collect())
    }
}

impl FulltextValues {
    pub fn into_edn(&self) -> edn::Value {
        edn::Value::Vector((&self.0).into_iter().map(|&(x, ref y)| edn::Value::Vector(vec![edn::Value::Integer(x), edn::Value::Text(y.clone())])).collect())
    }
}

/// Turn TypedValue::Ref into TypedValue::Keyword when it is possible.
trait ToIdent {
  fn map_ident(self, schema: &Schema) -> Self;
}

impl ToIdent for TypedValue {
    fn map_ident(self, schema: &Schema) -> Self {
        if let TypedValue::Ref(e) = self {
            schema.get_ident(e).cloned().map(|i| TypedValue::Keyword(Rc::new(i))).unwrap_or(TypedValue::Ref(e))
        } else {
            self
        }
    }
}

/// Convert a numeric entid to an ident `Entid` if possible, otherwise a numeric `Entid`.
fn to_entid(schema: &Schema, entid: i64) -> Entid {
    schema.get_ident(entid).map_or(Entid::Entid(entid), |ident| Entid::Ident(ident.clone()))
}

/// Return the set of datoms in the store, ordered by (e, a, v, tx), but not including any datoms of
/// the form [... :db/txInstant ...].
pub fn datoms<S: Borrow<Schema>>(conn: &rusqlite::Connection, schema: &S) -> Result<Datoms> {
    datoms_after(conn, schema, bootstrap::TX0 - 1)
}

/// Return the set of datoms in the store with transaction ID strictly greater than the given `tx`,
/// ordered by (e, a, v, tx).
///
/// The datom set returned does not include any datoms of the form [... :db/txInstant ...].
pub fn datoms_after<S: Borrow<Schema>>(conn: &rusqlite::Connection, schema: &S, tx: i64) -> Result<Datoms> {
    let borrowed_schema = schema.borrow();

    let mut stmt: rusqlite::Statement = conn.prepare("SELECT e, a, v, value_type_tag, tx FROM datoms WHERE tx > ? ORDER BY e ASC, a ASC, value_type_tag ASC, v ASC, tx ASC")?;

    let r: Result<Vec<_>> = stmt.query_and_then(&[&tx], |row| {
        let e: i64 = row.get_checked(0)?;
        let a: i64 = row.get_checked(1)?;

        if a == entids::DB_TX_INSTANT {
            return Ok(None);
        }

        let v: rusqlite::types::Value = row.get_checked(2)?;
        let value_type_tag: i32 = row.get_checked(3)?;

        let attribute = borrowed_schema.require_attribute_for_entid(a)?;
        let value_type_tag = if !attribute.fulltext { value_type_tag } else { ValueType::Long.value_type_tag() };

        let typed_value = TypedValue::from_sql_value_pair(v, value_type_tag)?.map_ident(borrowed_schema);
        let (value, _) = typed_value.to_edn_value_pair();

        let tx: i64 = row.get_checked(4)?;

        Ok(Some(Datom {
            e: Entid::Entid(e),
            a: to_entid(borrowed_schema, a),
            v: value,
            tx: tx,
            added: None,
        }))
    })?.collect();

    Ok(Datoms(r?.into_iter().filter_map(|x| x).collect()))
}

/// Return the sequence of transactions in the store with transaction ID strictly greater than the
/// given `tx`, ordered by (tx, e, a, v).
///
/// Each transaction returned includes the [:db/tx :db/txInstant ...] datom.
pub fn transactions_after<S: Borrow<Schema>>(conn: &rusqlite::Connection, schema: &S, tx: i64) -> Result<Transactions> {
    let borrowed_schema = schema.borrow();

    let mut stmt: rusqlite::Statement = conn.prepare("SELECT e, a, v, value_type_tag, tx, added FROM transactions WHERE tx > ? ORDER BY tx ASC, e ASC, a ASC, value_type_tag ASC, v ASC, added ASC")?;

    let r: Result<Vec<_>> = stmt.query_and_then(&[&tx], |row| {
        let e: i64 = row.get_checked(0)?;
        let a: i64 = row.get_checked(1)?;

        let v: rusqlite::types::Value = row.get_checked(2)?;
        let value_type_tag: i32 = row.get_checked(3)?;

        let attribute = borrowed_schema.require_attribute_for_entid(a)?;
        let value_type_tag = if !attribute.fulltext { value_type_tag } else { ValueType::Long.value_type_tag() };

        let typed_value = TypedValue::from_sql_value_pair(v, value_type_tag)?.map_ident(borrowed_schema);
        let (value, _) = typed_value.to_edn_value_pair();

        let tx: i64 = row.get_checked(4)?;
        let added: bool = row.get_checked(5)?;

        Ok(Datom {
            e: Entid::Entid(e),
            a: to_entid(borrowed_schema, a),
            v: value,
            tx: tx,
            added: Some(added),
        })
    })?.collect();

    // Group by tx.
    let r: Vec<Datoms> = r?.into_iter().group_by(|x| x.tx).into_iter().map(|(_key, group)| Datoms(group.collect())).collect();
    Ok(Transactions(r))
}

/// Return the set of fulltext values in the store, ordered by rowid.
pub fn fulltext_values(conn: &rusqlite::Connection) -> Result<FulltextValues> {
    let mut stmt: rusqlite::Statement = conn.prepare("SELECT rowid, text FROM fulltext_values ORDER BY rowid")?;

    let r: Result<Vec<_>> = stmt.query_and_then(&[], |row| {
        let rowid: i64 = row.get_checked(0)?;
        let text: String = row.get_checked(1)?;
        Ok((rowid, text))
    })?.collect();

    r.map(FulltextValues)
}

/// Execute the given `sql` query with the given `params` and format the results as a
/// tab-and-newline formatted string suitable for debug printing.
///
/// The query is printed followed by a newline, then the returned columns followed by a newline, and
/// then the data rows and columns.  All columns are aligned.
pub fn dump_sql_query(conn: &rusqlite::Connection, sql: &str, params: &[&ToSql]) -> Result<String> {
    let mut stmt: rusqlite::Statement = conn.prepare(sql)?;

    let mut tw = TabWriter::new(Vec::new()).padding(2);
    write!(&mut tw, "{}\n", sql).unwrap();

    for column_name in stmt.column_names() {
        write!(&mut tw, "{}\t", column_name).unwrap();
    }
    write!(&mut tw, "\n").unwrap();

    let r: Result<Vec<_>> = stmt.query_and_then(params, |row| {
        for i in 0..row.column_count() {
            let value: rusqlite::types::Value = row.get_checked(i)?;
            write!(&mut tw, "{:?}\t", value).unwrap();
        }
        write!(&mut tw, "\n").unwrap();
        Ok(())
    })?.collect();
    r?;

    let dump = String::from_utf8(tw.into_inner().unwrap()).unwrap();
    Ok(dump)
}
