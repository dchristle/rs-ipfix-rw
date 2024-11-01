//! IPFIX reader/writer

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    rc::Rc,
};

use ahash::{HashMap, HashMapExt};
use binrw::{
    binrw, binwrite, count,
    io::{Read, Seek, Write},
    until_eof, BinRead, BinReaderExt, BinResult, BinWrite, BinWriterExt, Endian,
};

use crate::information_elements::Formatter;
use crate::template_store::{Template, TemplateStore};
use crate::util::{stream_position, until_limit, write_position_at};

#[derive(derive_more::Display, Debug)]
pub enum IpfixError {
    #[display(fmt = "Missing Template")]
    MissingTemplate(u16),
    #[display(fmt = "Missing Data: {_0:?}")]
    MissingData(DataRecordKey),
    #[display(fmt = "Invalid Length for Field Spec: {ty:?}, {length}")]
    InvalidFieldSpecLength { ty: DataRecordType, length: u16 },
}

impl std::error::Error for IpfixError {}

impl IpfixError {
    pub(crate) fn into_binrw_error(self, pos: u64) -> binrw::Error {
        binrw::Error::Custom {
            pos,
            err: Box::new(self),
        }
    }
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.1>
#[binrw]
#[brw(big, magic = 10u16)]
#[br(import( templates: TemplateStore, formatter: Rc<Formatter>))]
#[bw(import( templates: TemplateStore, formatter: Rc<Formatter>, alignment: u8))]
#[bw(stream = s)]
#[derive(PartialEq, Clone, Debug)]
pub struct Message {
    #[br(temp)]
    // store offset for later updating
    #[bw(try_calc = stream_position(s))]
    length: u16,
    pub export_time: u32,
    pub sequence_number: u32,
    pub observation_domain_id: u32,
    #[br(parse_with = until_eof)]
    #[br(args(templates, formatter))]
    #[bw(args(templates, formatter, alignment))]
    pub sets: Vec<Set>,
    // jump back to length and set by current position
    #[br(temp)]
    #[bw(restore_position, try_calc = write_position_at(s, length, 0))]
    _temp: (),
}

impl Message {
    pub fn iter_template_records(&self) -> impl Iterator<Item = &TemplateRecord> {
        self.sets
            .iter()
            .filter_map(|set| match &set.records {
                Records::Template(templates) => Some(templates),
                _ => None,
            })
            .flatten()
    }

    pub fn iter_options_template_records(&self) -> impl Iterator<Item = &OptionsTemplateRecord> {
        self.sets
            .iter()
            .filter_map(|set| match &set.records {
                Records::OptionsTemplate(templates) => Some(templates),
                _ => None,
            })
            .flatten()
    }

    pub fn iter_data_records(&self) -> impl Iterator<Item = &DataRecord> {
        self.sets
            .iter()
            .filter_map(|set| match &set.records {
                Records::Data { data, .. } => Some(data),
                _ => None,
            })
            .flatten()
    }
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.3>
#[binrw]
#[br(big, import( templates: TemplateStore, formatter: Rc<Formatter> ))]
#[bw(big, stream = s, import( templates: TemplateStore, formatter: Rc<Formatter>, alignment: u8 ))]
#[derive(PartialEq, Clone, Debug)]
pub struct Set {
    #[br(temp)]
    #[bw(calc = records.set_id())]
    set_id: u16,
    #[br(temp)]
    #[br(assert(length > 4, "invalid set length: [{length} <= 4]"))]
    // store offset for later updating
    #[bw(try_calc = stream_position(s))]
    length: u16,
    #[br(pad_size_to = length - 4)]
    #[br(args(set_id, length - 4, templates, formatter))]
    #[bw(align_after = alignment)]
    #[bw(args(templates, formatter))]
    pub records: Records,
    // jump back to length and set by current position
    #[br(temp)]
    #[bw(restore_position, try_calc = write_position_at(s, length, length - 2))]
    _temp: (),
}

/// <https://www.rfc-editor.org/rfc/rfc7011.html#section-3.4>
#[binrw]
#[brw(big)]
#[br(import ( set_id: u16, length: u16, templates: TemplateStore, formatter: Rc<Formatter> ))]
#[bw(import ( templates: TemplateStore, formatter: Rc<Formatter> ))]
#[derive(PartialEq, Clone, Debug)]
pub enum Records {
    #[br(pre_assert(set_id == 2))]
    Template(
        #[br(map = |x: Vec<TemplateRecord>| {templates.insert_template_records(x.as_slice(), &formatter); x})]
        #[br(parse_with = until_limit(length.into()))]
        Vec<TemplateRecord>,
    ),
    #[br(pre_assert(set_id == 3))]
    OptionsTemplate(
        #[br(map = |x: Vec<OptionsTemplateRecord>| {templates.insert_options_template_records(x.as_slice(), &formatter); x})]
        #[br(parse_with = until_limit(length.into()))]
        Vec<OptionsTemplateRecord>,
    ),
    #[br(pre_assert(set_id > 255, "Set IDs 0-1 and 4-255 are reserved [set_id: {set_id}]"))]
    Data {
        #[br(calc = set_id)]
        #[bw(ignore)]
        set_id: u16,
        #[br(parse_with = until_limit(length.into()))]
        #[br(args(set_id, templates))]
        #[bw(args(*set_id, templates))]
        data: Vec<DataRecord>,
    },
}

impl Records {
    fn set_id(&self) -> u16 {
        match self {
            Self::Template(_) => 2,
            Self::OptionsTemplate(_) => 3,
            Self::Data { set_id, data: _ } => *set_id,
        }
    }
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.4.1>
#[binrw]
#[brw(big)]
#[derive(PartialEq, Clone, Debug)]
#[br(assert(template_id > 255, "Template IDs 0-255 are reserved [template_id: {template_id}]"))]
pub struct TemplateRecord {
    pub template_id: u16,
    #[br(temp)]
    #[bw(try_calc = field_specifiers.len().try_into())]
    field_count: u16,
    #[br(count = field_count)]
    pub field_specifiers: Vec<FieldSpecifier>,
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.4.2>
#[binrw]
#[brw(big)]
#[derive(PartialEq, Clone, Debug)]
#[br(assert(template_id > 255, "Template IDs 0-255 are reserved [template_id: {template_id}]"))]
pub struct OptionsTemplateRecord {
    pub template_id: u16,
    #[br(temp)]
    #[bw(try_calc = field_specifiers.len().try_into())]
    field_count: u16,
    // TODO
    pub scope_field_count: u16,
    #[br(count = field_count)]
    pub field_specifiers: Vec<FieldSpecifier>,
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.2>
#[binrw]
#[brw(big)]
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct FieldSpecifier {
    #[br(temp)]
    #[bw(calc = information_element_identifier | (u16::from(enterprise_number.is_some()) << 15))]
    raw_information_element_identifier: u16,
    #[br(calc = raw_information_element_identifier & (u16::MAX >> 1))]
    #[bw(ignore)]
    pub information_element_identifier: u16,
    pub field_length: u16,
    #[br(if(raw_information_element_identifier >> 15 == 1))]
    pub enterprise_number: Option<u32>,
}

impl FieldSpecifier {
    pub fn new(
        enterprise_number: Option<u32>,
        information_element_identifier: u16,
        field_length: u16,
    ) -> Self {
        Self {
            information_element_identifier,
            field_length,
            enterprise_number,
        }
    }
}

/// <https://www.rfc-editor.org/rfc/rfc7011#section-3.4.3>
#[derive(PartialEq, Clone, Debug)]
pub struct DataRecord {
    pub values: HashMap<DataRecordKey, DataRecordValue>,
}

/// slightly nicer syntax to make a `DataRecord`
#[macro_export]
macro_rules! data_record {
    { $($key:literal: $type:ident($value:expr)),+ $(,)? } => {
        DataRecord {
            values: HashMap::from_iter([
                $( ((DataRecordKey::Str($key), DataRecordValue::$type($value))), )+
            ])
        }
    };
}

impl BinRead for DataRecord {
    type Args<'a> = (u16, TemplateStore);

    fn read_options<R: Read + Seek>(
        reader: &mut R,
        endian: Endian,
        (set_id, templates): Self::Args<'_>,
    ) -> BinResult<Self> {
        let template = templates.get_template(set_id).ok_or(
            IpfixError::MissingTemplate(set_id).into_binrw_error(reader.stream_position()?),
        )?;

        // TODO: should these be handled differently?
        let field_specifiers = match template {
            Template::Template(field_specifiers) => field_specifiers,
            Template::OptionsTemplate(field_specifiers) => field_specifiers,
        };

        let mut values = HashMap::with_capacity(field_specifiers.len());
        for field_spec in field_specifiers.iter() {
            // TODO: should read whole field length according to template, regardless of type
            let value = reader.read_type_args(endian, (field_spec.ty, field_spec.field_length))?;

            values.insert(field_spec.name.clone(), value);
        }
        Ok(Self { values })
    }
}

impl BinWrite for DataRecord {
    type Args<'a> = (u16, TemplateStore);

    fn write_options<W: Write + Seek>(
        &self,
        writer: &mut W,
        endian: Endian,
        (set_id, templates): Self::Args<'_>,
    ) -> BinResult<()> {
        let template = templates.get_template(set_id).ok_or(
            IpfixError::MissingTemplate(set_id).into_binrw_error(writer.stream_position()?),
        )?;

        let field_specifiers = match template {
            Template::Template(field_specifiers) => field_specifiers,
            Template::OptionsTemplate(field_specifiers) => field_specifiers,
        };

        // TODO: should check if all keys are used?
        for field_spec in field_specifiers {
            // TODO: check template type vs actual type?
            let value = self.values.get(&field_spec.name).ok_or(
                IpfixError::MissingData(field_spec.name)
                    .into_binrw_error(writer.stream_position()?),
            )?;

            writer.write_type_args(value, endian, (field_spec.field_length,))?;
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum DataRecordKey {
    Str(&'static str),
    Unrecognized(FieldSpecifier),
    Err(String),
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum DataRecordType {
    UnsignedInt,
    SignedInt,
    Float,
    Bool,
    MacAddress,
    Bytes,
    String,
    DateTimeSeconds,
    DateTimeMilliseconds,
    DateTimeMicroseconds,
    DateTimeNanoseconds,
    Ipv4Addr,
    Ipv6Addr,
}

#[repr(C)]
#[derive(Debug, PartialEq, Clone)]
struct U40Bytes([u8; 5]);

impl BinWrite for U40Bytes {
    type Args<'a> = ();

    fn write_options<W: Write + Seek>(
        &self,
        writer: &mut W,
        _: Endian,
        _: Self::Args<'_>,
    ) -> BinResult<()> {
        let start_pos = writer.stream_position()?;
        writer.write_all(&self.0)?;
        let end_pos = writer.stream_position()?;
        assert_eq!(
            end_pos - start_pos,
            5,
            "U40Bytes wrote wrong number of bytes"
        );
        Ok(())
    }
}

impl BinRead for U40Bytes {
    type Args<'a> = ();

    fn read_options<R: Read + Seek>(
        reader: &mut R,
        _: Endian,
        _: Self::Args<'_>,
    ) -> BinResult<Self> {
        let mut bytes = [0u8; 5];
        reader.read_exact(&mut bytes)?;
        Ok(U40Bytes(bytes))
    }
}

#[binwrite]
#[bw(big)]
#[bw(import( length: u16 ))]
#[derive(PartialEq, Clone, Debug)]
pub enum DataRecordValue {
    U8(u8),
    U16(u16),
    U32(u32),
    U40(
        #[bw(try_map = |x: &u64| -> BinResult<U40Bytes> {
        if *x > 0xFF_FFFF_FFFF {
            return Err(binrw::Error::Custom {
                pos: 0,
                err: Box::new("Value too large for U40"),
            });
        }
        Ok(U40Bytes([
            ((*x >> 32) & 0xFF) as u8,
            ((*x >> 24) & 0xFF) as u8,
            ((*x >> 16) & 0xFF) as u8,
            ((*x >> 8) & 0xFF) as u8,
            (*x & 0xFF) as u8,
        ]))
    })]
        u64,
    ),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(#[bw(map = |&x| -> u8 {if x {1} else {2} })] bool),

    MacAddress([u8; 6]),

    // TODO: same logic as variable length string
    Bytes(
        #[bw(if(length == u16::MAX), calc = if self_2.len() < 255 { self_2.len() as u8 } else { 255 })]
         u8,
        #[bw(if(length == u16::MAX && self_2.len() >= 255), try_calc = self_2.len().try_into())]
        u16,
        Vec<u8>,
    ),
    String(
        #[bw(if(length == u16::MAX), calc = if self_2.len() < 255 { self_2.len() as u8 } else { 255 })]
         u8,
        #[bw(if(length == u16::MAX && self_2.len() >= 255), try_calc = self_2.len().try_into())]
        u16,
        #[bw(map = |x| x.as_bytes())] String,
    ),

    DateTimeSeconds(u32),
    DateTimeMilliseconds(u64),
    DateTimeMicroseconds(u64),
    DateTimeNanoseconds(u64),

    Ipv4Addr(#[bw(map = |&x| -> u32 {x.into()})] Ipv4Addr),
    Ipv6Addr(#[bw(map = |&x| -> u128 {x.into()})] Ipv6Addr),
}

fn read_variable_length<R: Read + Seek>(
    reader: &mut R,
    endian: Endian,
    length: u16,
) -> BinResult<Vec<u8>> {
    let actual_length = if length == u16::MAX {
        let var_length: u8 = reader.read_type(endian)?;
        if var_length == 255 {
            let var_length_ext: u16 = reader.read_type(endian)?;
            var_length_ext
        } else {
            var_length.into()
        }
    } else {
        length
    };
    count(actual_length.into())(reader, endian, ())
}

impl BinRead for DataRecordValue {
    type Args<'a> = (DataRecordType, u16);

    fn read_options<R: Read + Seek>(
        reader: &mut R,
        endian: Endian,
        (ty, length): Self::Args<'_>,
    ) -> BinResult<Self> {
        // TODO: length shouldn't actually change the data type, technically
        Ok(match (ty, length) {
            (DataRecordType::UnsignedInt, 1) => DataRecordValue::U8(reader.read_type(endian)?),
            (DataRecordType::UnsignedInt, 2) => DataRecordValue::U16(reader.read_type(endian)?),
            (DataRecordType::UnsignedInt, 4) => DataRecordValue::U32(reader.read_type(endian)?),
            (DataRecordType::UnsignedInt, 5) => DataRecordValue::U40(read_u40(reader)?),
            (DataRecordType::UnsignedInt, 8) => DataRecordValue::U64(reader.read_type(endian)?),
            (DataRecordType::SignedInt, 1) => DataRecordValue::I8(reader.read_type(endian)?),
            (DataRecordType::SignedInt, 2) => DataRecordValue::I16(reader.read_type(endian)?),
            (DataRecordType::SignedInt, 4) => DataRecordValue::I32(reader.read_type(endian)?),
            (DataRecordType::SignedInt, 8) => DataRecordValue::I64(reader.read_type(endian)?),
            (DataRecordType::Float, 4) => DataRecordValue::F32(reader.read_type(endian)?),
            (DataRecordType::Float, 8) => DataRecordValue::F64(reader.read_type(endian)?),
            // TODO: technically 1=>true, 2=>false, others undefined
            (DataRecordType::Bool, 1) => DataRecordValue::Bool(u8::read(reader).map(|x| x == 1)?),
            (DataRecordType::MacAddress, 6) => {
                DataRecordValue::MacAddress(reader.read_type(endian)?)
            }

            (DataRecordType::Bytes, _) => {
                DataRecordValue::Bytes(read_variable_length(reader, endian, length)?)
            }
            (DataRecordType::String, _) => DataRecordValue::String(
                match String::from_utf8(read_variable_length(reader, endian, length)?) {
                    Ok(s) => s,
                    Err(e) => {
                        return Err(binrw::Error::Custom {
                            pos: reader.stream_position()?,
                            err: Box::new(e),
                        });
                    }
                },
            ),

            (DataRecordType::DateTimeSeconds, 4) => {
                DataRecordValue::DateTimeSeconds(reader.read_type(endian)?)
            }

            (DataRecordType::DateTimeMilliseconds, 8) => {
                DataRecordValue::DateTimeMilliseconds(reader.read_type(endian)?)
            }

            (DataRecordType::DateTimeMicroseconds, 8) => {
                DataRecordValue::DateTimeMicroseconds(reader.read_type(endian)?)
            }

            (DataRecordType::DateTimeNanoseconds, 8) => {
                DataRecordValue::DateTimeNanoseconds(reader.read_type(endian)?)
            }

            (DataRecordType::Ipv4Addr, 4) => {
                DataRecordValue::Ipv4Addr(u32::read_be(reader)?.into())
            }

            (DataRecordType::Ipv6Addr, 16) => {
                DataRecordValue::Ipv6Addr(u128::read_be(reader)?.into())
            }
            _ => Err(IpfixError::InvalidFieldSpecLength { ty, length }
                .into_binrw_error(reader.stream_position()?))?,
        })
    }
}

fn read_u40<R: Read + Seek>(reader: &mut R) -> BinResult<u64> {
    let mut buf = [0u8; 5];
    reader.read_exact(&mut buf)?;

    // Convert 5 bytes to u64, maintaining network byte order (big-endian)
    let value = ((buf[0] as u64) << 32)
        | ((buf[1] as u64) << 24)
        | ((buf[2] as u64) << 16)
        | ((buf[3] as u64) << 8)
        | (buf[4] as u64);

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use binrw::BinRead;
    use std::io::Cursor;

    #[test]
    fn test_u40_edge_cases() {
        // Test zero
        let zero_value = DataRecordValue::U40(0);
        let mut writer = Cursor::new(Vec::new());
        zero_value
            .write_options(&mut writer, Endian::Big, (5,))
            .expect("Failed to write zero U40 value");
        let written_bytes = writer.into_inner();
        assert_eq!(written_bytes, [0, 0, 0, 0, 0]);

        // Test one
        let one_value = DataRecordValue::U40(1);
        let mut writer = Cursor::new(Vec::new());
        one_value
            .write_options(&mut writer, Endian::Big, (5,))
            .expect("Failed to write U40 value of 1");
        let written_bytes = writer.into_inner();
        assert_eq!(written_bytes, [0, 0, 0, 0, 1]);

        // Test max valid value (40 bits = 0xFFFFFFFFFF)
        let max_value = DataRecordValue::U40(0xFF_FFFF_FFFF);
        let mut writer = Cursor::new(Vec::new());
        max_value
            .write_options(&mut writer, Endian::Big, (5,))
            .expect("Failed to write max U40 value");
        let written_bytes = writer.into_inner();
        assert_eq!(written_bytes, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_u40_invalid_values() {
        // Test value that's too large (41 bits set)
        let too_large = DataRecordValue::U40(0x1FF_FFFF_FFFF);
        let mut writer = Cursor::new(Vec::new());
        let result = too_large.write_options(&mut writer, Endian::Big, (5,));
        assert!(
            result.is_err(),
            "Should fail to write value larger than 40 bits"
        );

        // Test value at exactly 41 bits
        let barely_too_large = DataRecordValue::U40(0x100_0000_0000);
        let mut writer = Cursor::new(Vec::new());
        let result = barely_too_large.write_options(&mut writer, Endian::Big, (5,));
        assert!(result.is_err(), "Should fail to write value at 41 bits");
    }

    #[test]
    fn test_u40_read_invalid() {
        // Test reading truncated data (only 4 bytes)
        let truncated_data = vec![0xFF, 0xFF, 0xFF, 0xFF];
        let mut reader = Cursor::new(truncated_data);
        let result = DataRecordValue::read_options(
            &mut reader,
            Endian::Big,
            (DataRecordType::UnsignedInt, 5), // Specifically asking for 5 bytes
        );
        assert!(result.is_err(), "Should fail to read truncated data");
    }

    #[test]
    fn test_u40_roundtrip() {
        let test_values = vec![
            0u64,
            1u64,
            0xFF_FFFF_FFFFu64,   // max value
            0x11_2233_4455u64,   // typical value
            0x000F_FFFF_FFFFu64, // high bits not set
            0x00F0_0000_0000u64, // high bits set but still within 40 bits
        ];

        for value in test_values {
            let original = DataRecordValue::U40(value);

            let mut writer = Cursor::new(Vec::new());
            original
                .write_options(&mut writer, Endian::Big, (5,))
                .expect(&format!("Failed to write U40 value {:#X}", value));

            // Verify we wrote exactly 5 bytes
            let written_bytes = writer.into_inner();
            assert_eq!(written_bytes.len(), 5, "Should write exactly 5 bytes");

            let mut reader = Cursor::new(written_bytes);
            let read_value = DataRecordValue::read_options(
                &mut reader,
                Endian::Big,
                (DataRecordType::UnsignedInt, 5),
            )
            .expect(&format!("Failed to read U40 value {:#X}", value));

            assert_eq!(
                read_value, original,
                "Roundtrip failed for value {:#X}",
                value
            );

            // Verify we read exactly 5 bytes
            assert_eq!(reader.position(), 5, "Should read exactly 5 bytes");
        }
    }
}
