//! Persistence for invoices and structured extractions (task 73).
//!
//! Split from [`invoice`](crate::extract::invoice) and
//! [`structured`](crate::extract::structured) so those two files hold parsing
//! and nothing else, and every statement that touches `invoices`,
//! `invoice_line_items` and `structured_extractions` is in one place where the
//! column list can be read against `V49__invoices.sql`.
//!
//! # Re-extraction replaces
//!
//! A source document is immutable, so a second extraction of it is a better
//! reading of the same thing rather than a new fact. [`save_invoice`] deletes
//! and rewrites the `(message_id, part_id)` row and its lines inside one
//! transaction; the alternative — accumulating revisions — would make
//! "what is this invoice's total" a question with several answers.
//!
//! # Provenance survives the round trip
//!
//! `invoices.provenance` is written from
//! [`Invoice::provenance`](crate::extract::invoice::Invoice::provenance) and
//! read back into the same map, so a row that comes out of the database says
//! exactly what the extraction said: which part, which page, which bytes, and
//! parsed or inferred. A stored invoice that had lost its provenance would be
//! a number with no way to check it, which is the thing this feature is not
//! allowed to produce.

use std::collections::BTreeMap;

use rusqlite::OptionalExtension;
use serde_json::Value;

use crate::error::Error;
use crate::extract::invoice::{
    Claim, DocKind, Invoice, LineItem, Money, Origin, PaymentStatus, Provenance, StoredInvoice,
};
use crate::extract::structured::Extraction;
use crate::storage::Database;

/// Most invoices one query returns.
pub const MAX_INVOICE_ROWS: i64 = 500;

/// Default page size when a caller asks for none.
pub const DEFAULT_INVOICE_ROWS: i64 = 50;

/// Which stored invoices to return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvoiceFilter {
    /// Restrict to one account; `None` means every configured account.
    pub account_id: Option<i64>,
    /// Restrict to one message.
    pub message_id: Option<i64>,
    /// Case-insensitive substring of the vendor.
    pub vendor: Option<String>,
    /// Only invoices issued at or after this instant.
    pub since: Option<i64>,
    /// Only invoices issued at or before this instant.
    pub until: Option<i64>,
    /// Page size, clamped to [`MAX_INVOICE_ROWS`].
    pub limit: i64,
}

/// Write one extraction, replacing any previous reading of the same document.
///
/// # Errors
///
/// A mapped storage error. [`Error::FailedPrecondition`] if the extraction
/// found nothing worth storing — a row of all-`NULL` columns is a fiction, and
/// a caller must be able to tell "this document has no invoice in it" from
/// "this invoice has no total".
pub async fn save_invoice(
    db: &Database,
    message_id: i64,
    invoice: &Invoice,
) -> Result<StoredInvoice, Error> {
    if invoice.is_empty() {
        return Err(Error::failed_precondition(
            "no invoice fields could be read from this document".to_owned(),
        ));
    }
    let invoice = &single_currency(invoice);
    let provenance = serde_json::to_string(&provenance_json(invoice))
        .map_err(|e| Error::internal(format!("invoice provenance could not be encoded: {e}")))?;
    let warnings = serde_json::to_string(&invoice.warnings)
        .map_err(|e| Error::internal(format!("invoice warnings could not be encoded: {e}")))?;
    let stored = invoice.clone();
    let invoice = invoice.clone();
    let (invoice_id, extracted_at) = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            // Deleted rather than upserted: the line items belong to the row
            // id, and an upsert would leave the *previous* reading's lines
            // attached to the new total.
            tx.execute(
                "DELETE FROM invoices WHERE message_id = ?1 AND part_id = ?2",
                rusqlite::params![message_id, &invoice.part],
            )?;
            tx.execute(
                "INSERT INTO invoices (
                     message_id, part_id, doc_kind, vendor, number, currency,
                     subtotal_minor, tax_minor, total_minor, issued_at, due_at,
                     status, inferred, provenance, warnings
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    message_id,
                    &invoice.part,
                    invoice.kind.as_str(),
                    invoice.vendor.as_ref().map(|c| c.value.clone()),
                    invoice.number.as_ref().map(|c| c.value.clone()),
                    invoice.currency.clone(),
                    invoice.subtotal.as_ref().map(|c| c.value.minor_units),
                    invoice.tax.as_ref().map(|c| c.value.minor_units),
                    invoice.total.as_ref().map(|c| c.value.minor_units),
                    invoice.issued_at.as_ref().map(|c| c.value),
                    invoice.due_at.as_ref().map(|c| c.value),
                    invoice.status.as_ref().map(|c| c.value.as_str()),
                    i64::from(invoice.inferred()),
                    &provenance,
                    &warnings,
                ],
            )?;
            let invoice_id = tx.last_insert_rowid();
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO invoice_line_items (
                         invoice_id, position, description, quantity,
                         unit_price_minor, total_minor, origin
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for (position, item) in invoice.line_items.iter().enumerate() {
                    stmt.execute(rusqlite::params![
                        invoice_id,
                        i64::try_from(position).unwrap_or(i64::MAX),
                        &item.description,
                        item.quantity,
                        item.unit_price.as_ref().map(|m| m.minor_units),
                        item.total.as_ref().map(|m| m.minor_units),
                        item.origin.as_str(),
                    ])?;
                }
            }
            let extracted_at: i64 = tx.query_row(
                "SELECT extracted_at FROM invoices WHERE invoice_id = ?1",
                [invoice_id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok((invoice_id, extracted_at))
        })
        .await?;

    Ok(StoredInvoice {
        invoice_id,
        message_id,
        extracted_at,
        invoice: stored,
    })
}

/// Drop any amount that is not in the row's own currency, and say so.
///
/// `invoices` has one `currency` column and three amount columns, so an amount
/// in a second currency cannot be stored without being re-labelled when it is
/// read back — and a re-labelled amount is a *changed number*. A document that
/// states `Subtotal $100.00 / Tax £10.00` is a real thing to receive (a badly
/// built template, or a deliberate one), and storing that tax as `10.00` under
/// `currency = USD` would turn a mismatch this module already warns about into
/// a silently wrong figure. So it is not stored, and the warning says which
/// amount was dropped and why.
///
/// The alternative — a currency column per amount — buys the ability to store
/// something nobody can sum. Refusing is the honest shape, and the document is
/// still in the mailbox.
fn single_currency(invoice: &Invoice) -> Invoice {
    let mut out = invoice.clone();
    let row = out.currency.clone();
    let drop_foreign =
        |slot: &mut Option<Claim<Money>>, field: &str, warnings: &mut Vec<String>| {
            let Some(claim) = slot.as_ref() else { return };
            if Some(claim.value.currency.as_str()) == row.as_deref() {
                return;
            }
            warnings.push(format!(
                "the {field} {} is not in this document's currency ({}), and one row stores one \
             currency; it was not stored",
                claim.value.display(),
                row.as_deref().unwrap_or("none")
            ));
            *slot = None;
        };
    // Warnings are collected separately and appended: the closure cannot hold
    // `&mut out.warnings` while a caller also hands it `&mut out.subtotal`.
    let mut warnings = Vec::new();
    drop_foreign(&mut out.subtotal, "subtotal", &mut warnings);
    drop_foreign(&mut out.tax, "tax", &mut warnings);
    drop_foreign(&mut out.total, "total", &mut warnings);

    let mut dropped_lines = 0usize;
    for item in &mut out.line_items {
        for money in [&mut item.unit_price, &mut item.total] {
            if money
                .as_ref()
                .is_some_and(|m| Some(m.currency.as_str()) != row.as_deref())
            {
                *money = None;
                dropped_lines += 1;
            }
        }
    }
    if dropped_lines > 0 {
        warnings.push(format!(
            "{dropped_lines} line-item amount(s) were in a currency other than this \
             document's ({}) and were not stored",
            row.as_deref().unwrap_or("none")
        ));
    }
    for warning in warnings {
        if !out.warnings.contains(&warning) {
            out.warnings.push(warning);
        }
    }
    out
}

/// Read stored invoices, newest extraction first.
///
/// # Errors
///
/// A mapped storage error.
pub async fn list_invoices(
    db: &Database,
    filter: &InvoiceFilter,
) -> Result<Vec<StoredInvoice>, Error> {
    let limit = if filter.limit <= 0 {
        DEFAULT_INVOICE_ROWS
    } else {
        filter.limit.min(MAX_INVOICE_ROWS)
    };
    let filter = filter.clone();
    let rows: Vec<Row> = db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT i.invoice_id, i.message_id, i.part_id, i.doc_kind, i.vendor,
                        i.number, i.currency, i.subtotal_minor, i.tax_minor,
                        i.total_minor, i.issued_at, i.due_at, i.status,
                        i.provenance, i.warnings, i.extracted_at
                   FROM invoices i
                   JOIN messages m ON m.id = i.message_id
                  WHERE (?1 IS NULL OR m.account_id = ?1)
                    AND (?2 IS NULL OR i.message_id = ?2)
                    AND (?3 IS NULL OR (i.vendor IS NOT NULL
                                        AND instr(lower(i.vendor), lower(?3)) > 0))
                    AND (?4 IS NULL OR (i.issued_at IS NOT NULL AND i.issued_at >= ?4))
                    AND (?5 IS NULL OR (i.issued_at IS NOT NULL AND i.issued_at <= ?5))
                  ORDER BY i.extracted_at DESC, i.invoice_id DESC
                  LIMIT ?6",
            )?;
            let mapped = stmt
                .query_map(
                    rusqlite::params![
                        filter.account_id,
                        filter.message_id,
                        filter.vendor,
                        filter.since,
                        filter.until,
                        limit,
                    ],
                    |row| {
                        Ok(Row {
                            invoice_id: row.get(0)?,
                            message_id: row.get(1)?,
                            part_id: row.get(2)?,
                            doc_kind: row.get(3)?,
                            vendor: row.get(4)?,
                            number: row.get(5)?,
                            currency: row.get(6)?,
                            subtotal_minor: row.get(7)?,
                            tax_minor: row.get(8)?,
                            total_minor: row.get(9)?,
                            issued_at: row.get(10)?,
                            due_at: row.get(11)?,
                            status: row.get(12)?,
                            provenance: row.get(13)?,
                            warnings: row.get(14)?,
                            extracted_at: row.get(15)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(mapped)
        })
        .await?;

    let ids: Vec<i64> = rows.iter().map(|row| row.invoice_id).collect();
    let lines = line_items(db, ids).await?;
    rows.into_iter()
        .map(|row| {
            let items = lines.get(&row.invoice_id).cloned().unwrap_or_default();
            row.into_stored(items)
        })
        .collect()
}

/// Every stored line item for `ids`, keyed by invoice.
async fn line_items(db: &Database, ids: Vec<i64>) -> Result<BTreeMap<i64, Vec<LineItem>>, Error> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    // The currency lives on the parent row, so it is resolved after the read
    // rather than joined here; a line item's minor units are meaningless
    // without it and meaningless twice if the join disagreed with the parent.
    let rows: Vec<LineRow> = db
        .read(move |conn| {
            let mut out = Vec::new();
            let mut stmt = conn.prepare(
                "SELECT l.invoice_id, l.description, l.quantity, l.unit_price_minor,
                        l.total_minor, l.origin, i.currency
                   FROM invoice_line_items l
                   JOIN invoices i ON i.invoice_id = l.invoice_id
                  WHERE l.invoice_id = ?1
                  ORDER BY l.position",
            )?;
            for id in ids {
                let mapped = stmt
                    .query_map([id], |row| {
                        Ok(LineRow {
                            invoice_id: row.get(0)?,
                            description: row.get(1)?,
                            quantity: row.get(2)?,
                            unit_price_minor: row.get(3)?,
                            total_minor: row.get(4)?,
                            origin: row.get(5)?,
                            currency: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                out.extend(mapped);
            }
            Ok(out)
        })
        .await?;

    let mut by_invoice: BTreeMap<i64, Vec<LineItem>> = BTreeMap::new();
    for row in rows {
        let currency = row.currency;
        let money = |minor: Option<i64>| -> Option<Money> {
            Some(Money {
                currency: currency.clone()?,
                minor_units: minor?,
            })
        };
        by_invoice
            .entry(row.invoice_id)
            .or_default()
            .push(LineItem {
                description: row.description,
                quantity: row.quantity,
                unit_price: money(row.unit_price_minor),
                total: money(row.total_minor),
                // A row whose origin this build has no variant for is treated as
                // inferred. The safe direction: presenting a parsed field as
                // inferred understates a claim, and the reverse overstates one.
                origin: Origin::parse(&row.origin).unwrap_or(Origin::Model),
            });
    }
    Ok(by_invoice)
}

/// One `invoice_line_items` row, joined to its parent's currency.
struct LineRow {
    invoice_id: i64,
    description: String,
    quantity: Option<f64>,
    unit_price_minor: Option<i64>,
    total_minor: Option<i64>,
    origin: String,
    currency: Option<String>,
}

/// One `invoices` row as read.
struct Row {
    invoice_id: i64,
    message_id: i64,
    part_id: String,
    doc_kind: String,
    vendor: Option<String>,
    number: Option<String>,
    currency: Option<String>,
    subtotal_minor: Option<i64>,
    tax_minor: Option<i64>,
    total_minor: Option<i64>,
    issued_at: Option<i64>,
    due_at: Option<i64>,
    status: Option<String>,
    provenance: String,
    warnings: String,
    extracted_at: i64,
}

impl Row {
    fn into_stored(self, line_items: Vec<LineItem>) -> Result<StoredInvoice, Error> {
        let kind = DocKind::parse(&self.doc_kind).ok_or_else(|| {
            Error::internal(format!(
                "invoice {} has a document kind no version of this code wrote: {}",
                self.invoice_id, self.doc_kind
            ))
        })?;
        let provenance: BTreeMap<String, StoredProvenance> =
            match serde_json::from_str(&self.provenance) {
                Ok(map) => map,
                Err(error) => {
                    // Logged, not swallowed: a row whose provenance will not decode
                    // is a row whose every claim is about to be reported as
                    // unchecked, and an operator has to be able to find out why.
                    tracing::warn!(
                        %error,
                        invoice_id = self.invoice_id,
                        "an invoice's provenance did not decode; every field reads as inferred"
                    );
                    BTreeMap::new()
                }
            };
        let warnings: Vec<String> = serde_json::from_str(&self.warnings).unwrap_or_default();
        let currency = self.currency.clone();

        // A field with no recorded provenance is reported as **inferred**, not
        // as parsed. `Origin::default()` is `Parsed` — right for a producer
        // building a claim it just read, and exactly wrong here, where the
        // absence means "this row does not say". Understating a claim is the
        // safe direction; overstating one puts a model's guess in front of a
        // reader as the document's own words.
        let whence = |field: &str| -> Provenance {
            provenance
                .get(field)
                .map(StoredProvenance::to_provenance)
                .unwrap_or(Provenance {
                    origin: Origin::Model,
                    ..Provenance::default()
                })
        };

        let text = |value: Option<String>, field: &str| -> Option<Claim<String>> {
            Some(Claim {
                value: value?,
                provenance: whence(field),
            })
        };
        let money = |minor: Option<i64>, field: &str| -> Option<Claim<Money>> {
            Some(Claim {
                value: Money {
                    currency: currency.clone()?,
                    minor_units: minor?,
                },
                provenance: whence(field),
            })
        };
        let date = |at: Option<i64>, field: &str| -> Option<Claim<i64>> {
            Some(Claim {
                value: at?,
                provenance: whence(field),
            })
        };

        Ok(StoredInvoice {
            invoice_id: self.invoice_id,
            message_id: self.message_id,
            extracted_at: self.extracted_at,
            invoice: Invoice {
                kind,
                part: self.part_id,
                vendor: text(self.vendor, "vendor"),
                number: text(self.number, "number"),
                currency: self.currency,
                subtotal: money(self.subtotal_minor, "subtotal"),
                tax: money(self.tax_minor, "tax"),
                total: money(self.total_minor, "total"),
                issued_at: date(self.issued_at, "issued_at"),
                due_at: date(self.due_at, "due_at"),
                status: self
                    .status
                    .as_deref()
                    .and_then(PaymentStatus::parse)
                    .map(|value| Claim {
                        value,
                        provenance: whence("status"),
                    }),
                line_items,
                warnings,
            },
        })
    }
}

/// [`Provenance`] on its way to and from `invoices.provenance`.
///
/// A serde mirror rather than derives on `Provenance` itself: the stored form
/// is a wire contract this file owns, and deriving `Serialize` on the domain
/// type would let a rename in the domain silently change what every existing
/// row means.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct StoredProvenance {
    #[serde(default)]
    part: String,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    span_start: usize,
    #[serde(default)]
    span_end: usize,
    #[serde(default)]
    origin: String,
}

impl StoredProvenance {
    fn to_provenance(&self) -> Provenance {
        Provenance {
            part: self.part.clone(),
            page: self.page,
            span_start: self.span_start,
            span_end: self.span_end,
            // Same direction as the line-item case above: an origin this build
            // cannot name is treated as inferred rather than as read.
            origin: Origin::parse(&self.origin).unwrap_or(Origin::Model),
        }
    }
}

fn provenance_json(invoice: &Invoice) -> BTreeMap<String, StoredProvenance> {
    invoice
        .provenance()
        .into_iter()
        .map(|(field, provenance)| {
            (
                field,
                StoredProvenance {
                    part: provenance.part,
                    page: provenance.page,
                    span_start: provenance.span_start,
                    span_end: provenance.span_end,
                    origin: provenance.origin.as_str().to_owned(),
                },
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Structured extractions
// ---------------------------------------------------------------------------

/// Write one validated document, replacing any previous extraction of the same
/// message under the same schema.
///
/// # Errors
///
/// A mapped storage error, or [`Error::Internal`] if the already-validated
/// document cannot be serialized.
pub async fn save_extraction(
    db: &Database,
    message_id: i64,
    schema_name: &str,
    schema_hash: &str,
    model: &str,
    data: &Value,
) -> Result<Extraction, Error> {
    let data = serde_json::to_string(data)
        .map_err(|e| Error::internal(format!("a validated extraction would not serialize: {e}")))?;
    let schema_name = schema_name.to_owned();
    let schema_hash = schema_hash.to_owned();
    let model = model.to_owned();
    let stored = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM structured_extractions
                  WHERE message_id = ?1 AND schema_name = ?2 AND schema_hash = ?3",
                rusqlite::params![message_id, &schema_name, &schema_hash],
            )?;
            tx.execute(
                "INSERT INTO structured_extractions
                     (message_id, schema_name, schema_hash, data, model)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![message_id, &schema_name, &schema_hash, &data, &model],
            )?;
            let extraction_id = tx.last_insert_rowid();
            let created_at: i64 = tx.query_row(
                "SELECT created_at FROM structured_extractions WHERE extraction_id = ?1",
                [extraction_id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok(Extraction {
                extraction_id,
                message_id,
                schema_name,
                schema_hash,
                data,
                model,
                created_at,
            })
        })
        .await?;
    Ok(stored)
}

/// The stored extraction for `(message, schema, hash)`, if there is one.
///
/// # Errors
///
/// A mapped storage error.
pub async fn find_extraction(
    db: &Database,
    message_id: i64,
    schema_name: &str,
    schema_hash: &str,
) -> Result<Option<Extraction>, Error> {
    let schema_name = schema_name.to_owned();
    let schema_hash = schema_hash.to_owned();
    let found = db
        .read(move |conn| {
            conn.query_row(
                "SELECT extraction_id, data, model, created_at
                   FROM structured_extractions
                  WHERE message_id = ?1 AND schema_name = ?2 AND schema_hash = ?3",
                rusqlite::params![message_id, &schema_name, &schema_hash],
                |row| {
                    Ok(Extraction {
                        extraction_id: row.get(0)?,
                        message_id,
                        schema_name: schema_name.clone(),
                        schema_hash: schema_hash.clone(),
                        data: row.get(1)?,
                        model: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await?;
    Ok(found)
}
