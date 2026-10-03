//! Protocol 2 scalar wincode schema. Variable fields are length-checked
//! against actual staged bytes before allocation; no default container cap.

use crate::bridge::{Action, ActionResult, Snapshot, Update, Wire};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) fn put<T>(out: &mut impl Write, value: &T) -> anyhow::Result<()>
where
    T: wincode::SchemaWrite<wincode::config::DefaultConfig, Src = T>,
{
    T::write(wincode::io::std_write::WriteAdapter::new(out), value)?;
    Ok(())
}

fn text(out: &mut impl Write, value: &str) -> anyhow::Result<()> {
    put(out, &(value.len() as u64))?;
    out.write_all(value.as_bytes())?;
    Ok(())
}

fn value(out: &mut impl Write, root: &Value) -> anyhow::Result<()> {
    // Keep traversal metadata per nesting level, never one reference per
    // child of a large array. The owned source Value is the only full image.
    enum Part<'a> {
        Value(&'a Value),
        Array(std::slice::Iter<'a, Value>),
        Object(serde_json::map::Iter<'a>),
    }
    let mut stack = vec![Part::Value(root)];
    while let Some(part) = stack.pop() {
        let v = match part {
            Part::Array(mut children) => {
                let Some(v) = children.next() else {
                    continue;
                };
                stack.push(Part::Array(children));
                v
            }
            Part::Object(mut children) => {
                let Some((key, v)) = children.next() else {
                    continue;
                };
                text(out, key)?;
                stack.push(Part::Object(children));
                v
            }
            Part::Value(v) => v,
        };
        match v {
            Value::Null => put(out, &0u8)?,
            Value::Bool(v) => {
                put(out, &1u8)?;
                put(out, v)?;
            }
            Value::Number(v) => {
                if let Some(n) = v.as_u64() {
                    put(out, &2u8)?;
                    put(out, &n)?;
                } else if let Some(n) = v.as_i64() {
                    put(out, &3u8)?;
                    put(out, &n)?;
                } else {
                    put(out, &4u8)?;
                    put(
                        out,
                        &v.as_f64()
                            .ok_or_else(|| anyhow::anyhow!("invalid number"))?,
                    )?;
                }
            }
            Value::String(v) => {
                put(out, &5u8)?;
                text(out, v)?;
            }
            Value::Array(v) => {
                put(out, &6u8)?;
                put(out, &(v.len() as u64))?;
                stack.push(Part::Array(v.iter()));
            }
            Value::Object(v) => {
                put(out, &7u8)?;
                put(out, &(v.len() as u64))?;
                stack.push(Part::Object(v.iter()));
            }
        }
    }
    Ok(())
}

pub(crate) fn encode(out: &mut impl Write, wire: &Wire) -> anyhow::Result<()> {
    match wire {
        Wire::Snapshot { snapshot } => {
            put(out, &0u8)?;
            put(out, &snapshot.sequence)?;
            value(out, &snapshot.state)?;
        }
        Wire::Update { update } => {
            put(out, &1u8)?;
            put(out, &update.sequence)?;
            value(out, &update.state)?;
        }
        Wire::Applied { sequence } => {
            put(out, &2u8)?;
            put(out, sequence)?;
        }
        Wire::Request { id, action } => {
            put(out, &3u8)?;
            put(out, id)?;
            text(out, &action.kind)?;
            value(out, &action.input)?;
        }
        Wire::Result { id, result } => {
            put(out, &4u8)?;
            put(out, id)?;
            match result {
                ActionResult::Completed { value: v } => {
                    put(out, &0u8)?;
                    value(out, v)?;
                }
                ActionResult::Refused { why } => {
                    put(out, &1u8)?;
                    text(out, why)?;
                }
            }
        }
        Wire::UploadBegin {
            id,
            media_type,
            size,
        } => {
            put(out, &5u8)?;
            put(out, id)?;
            text(out, media_type)?;
            put(out, &(*size as u64))?;
        }
        Wire::UploadChunk { bytes } => {
            put(out, &6u8)?;
            put(out, &(bytes.len() as u64))?;
            out.write_all(bytes)?;
        }
        Wire::UploadEnd { id } => {
            put(out, &7u8)?;
            put(out, id)?;
        }
        Wire::Failure { code, why } => {
            put(out, &8u8)?;
            text(out, code)?;
            text(out, why)?;
        }
        Wire::Resync => put(out, &9u8)?,
        _ => anyhow::bail!("bootstrap message in binary phase"),
    }
    Ok(())
}

pub(crate) struct Input<R> {
    reader: R,
    remaining: u64,
    cancelled: Arc<AtomicBool>,
}

impl<R: Read> Read for Input<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::other("transfer cancelled"));
        }
        let limit = bytes
            .len()
            .min(crate::bridge::BULK_CHUNK_BYTES)
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.reader.read(&mut bytes[..limit])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl<R: Read> Input<R> {
    fn scalar<T>(&mut self) -> anyhow::Result<T>
    where
        T: for<'a> wincode::SchemaRead<'a, wincode::config::DefaultConfig, Dst = T>,
    {
        Ok(wincode::deserialize_from(
            wincode::io::std_read::ReadAdapter::new(self),
        )?)
    }
    fn count(&mut self) -> anyhow::Result<usize> {
        let n: u64 = self.scalar()?;
        anyhow::ensure!(
            n <= self.remaining,
            "length exceeds available transfer bytes"
        );
        Ok(usize::try_from(n)?)
    }
    fn read_bytes(&mut self) -> anyhow::Result<Vec<u8>> {
        let len = self.count()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len)?;
        bytes.resize(len, 0);
        self.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    fn text(&mut self) -> anyhow::Result<String> {
        Ok(String::from_utf8(self.read_bytes()?)?)
    }
    fn value(&mut self) -> anyhow::Result<Value> {
        enum Container {
            Array(usize, Vec<Value>),
            Object(usize, serde_json::Map<String, Value>, String),
        }
        let mut stack = Vec::new();
        loop {
            let mut v = match self.scalar::<u8>()? {
                0 => Value::Null,
                1 => Value::Bool(self.scalar()?),
                2 => Value::from(self.scalar::<u64>()?),
                3 => Value::from(self.scalar::<i64>()?),
                4 => Value::Number(
                    serde_json::Number::from_f64(self.scalar()?)
                        .ok_or_else(|| anyhow::anyhow!("nonfinite number"))?,
                ),
                5 => Value::String(self.text()?),
                6 => {
                    let count = self.count()?;
                    if count == 0 {
                        Value::Array(Vec::new())
                    } else {
                        stack.push(Container::Array(count, Vec::new()));
                        continue;
                    }
                }
                7 => {
                    let count = self.count()?;
                    if count == 0 {
                        Value::Object(Default::default())
                    } else {
                        let key = self.text()?;
                        stack.push(Container::Object(count, Default::default(), key));
                        continue;
                    }
                }
                _ => anyhow::bail!("unknown binary value tag"),
            };
            loop {
                match stack.pop() {
                    None => return Ok(v),
                    Some(Container::Array(left, mut values)) => {
                        values.try_reserve(1)?;
                        values.push(v);
                        if left == 1 {
                            v = Value::Array(values);
                        } else {
                            stack.push(Container::Array(left - 1, values));
                            break;
                        }
                    }
                    Some(Container::Object(left, mut values, key)) => {
                        anyhow::ensure!(values.insert(key, v).is_none(), "duplicate object key");
                        if left == 1 {
                            v = Value::Object(values);
                        } else {
                            let key = self.text()?;
                            stack.push(Container::Object(left - 1, values, key));
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn decode<R: Read>(
    reader: R,
    bytes: u64,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<Wire> {
    let mut input = Input {
        reader,
        remaining: bytes,
        cancelled,
    };
    let wire = match input.scalar::<u8>()? {
        0 => Wire::Snapshot {
            snapshot: Snapshot {
                sequence: input.scalar()?,
                state: input.value()?,
            },
        },
        1 => Wire::Update {
            update: Update {
                sequence: input.scalar()?,
                state: input.value()?,
            },
        },
        2 => Wire::Applied {
            sequence: input.scalar()?,
        },
        3 => Wire::Request {
            id: input.scalar()?,
            action: Action {
                kind: input.text()?,
                input: input.value()?,
            },
        },
        4 => {
            let id = input.scalar()?;
            let result = match input.scalar::<u8>()? {
                0 => ActionResult::Completed {
                    value: input.value()?,
                },
                1 => ActionResult::Refused { why: input.text()? },
                _ => anyhow::bail!("unknown result tag"),
            };
            Wire::Result { id, result }
        }
        5 => Wire::UploadBegin {
            id: input.scalar()?,
            media_type: input.text()?,
            size: usize::try_from(input.scalar::<u64>()?)?,
        },
        6 => Wire::UploadChunk {
            bytes: input.read_bytes()?,
        },
        7 => Wire::UploadEnd {
            id: input.scalar()?,
        },
        8 => Wire::Failure {
            code: input.text()?,
            why: input.text()?,
        },
        9 => Wire::Resync,
        _ => anyhow::bail!("unknown wire message tag"),
    };
    anyhow::ensure!(input.remaining == 0, "trailing transfer bytes");
    Ok(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_values_preserve_numbers_unicode_and_large_text_without_default_caps() {
        let wire = Wire::Request {
            id: 7,
            action: Action {
                kind: "test".into(),
                input: serde_json::json!([
                    null,
                    true,
                    -5,
                    u64::MAX,
                    1.25,
                    "\"\\\n🦀",
                    "x".repeat(5 * 1024 * 1024)
                ]),
            },
        };
        let mut bytes = Vec::new();
        encode(&mut bytes, &wire).unwrap();
        assert_eq!(
            decode(
                &bytes[..],
                bytes.len() as u64,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap(),
            wire
        );
        let mut forged = vec![3u8];
        put(&mut forged, &7u64).unwrap();
        put(&mut forged, &u64::MAX).unwrap();
        assert!(
            decode(
                &forged[..],
                forged.len() as u64,
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
    }
    #[test]
    fn browser_binary_encoding_removes_outer_string_escaping() {
        let data = serde_json::json!({"text":"\"\\\n".repeat(1000)});
        let wire = Wire::Update {
            update: Update {
                sequence: 1,
                state: serde_json::json!({"live":[{"event":"status","data":data}]}),
            },
        };
        let mut bytes = Vec::new();
        encode(&mut bytes, &wire).unwrap();
        assert_eq!(bytes.len(), 3118);
        assert_eq!(
            decode(
                &bytes[..],
                bytes.len() as u64,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap(),
            wire
        );
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;
    #[test]
    fn logical_schema_has_stable_tags_and_rejects_invalid_lengths() {
        let mut bytes = Vec::new();
        encode(&mut bytes, &Wire::Applied { sequence: 2 }).unwrap();
        assert_eq!(bytes, [2, 2, 0, 0, 0, 0, 0, 0, 0]);
        bytes.clear();
        encode(
            &mut bytes,
            &Wire::Snapshot {
                snapshot: Snapshot {
                    sequence: 1,
                    state: Value::Null,
                },
            },
        )
        .unwrap();
        assert_eq!(bytes, [0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        for bad in [vec![1, 0, 0], vec![9, 0], vec![255]] {
            assert!(decode(&bad[..], bad.len() as u64, Arc::new(AtomicBool::new(false))).is_err());
        }
    }
}
