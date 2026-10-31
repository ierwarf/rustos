//! Canonical bounded TLV payloads for staged Ring3 control protocols.
//! Existing production wire structures are not retired by adding this parser.
//! Adapters must reject the old wire only AFTER all producers are cut over.
use alloc::vec::Vec;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Short, Overflow, Capacity, Noncanonical, UnknownField,
    DuplicateField, RequiredField, WrongVersion, WrongOperation,
    WrongRequest, WrongPrincipal, StaleEpoch, Replay, Allocation,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Field<'a>{pub tag:u16,pub value:&'a [u8]}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Descriptor {
    pub tag:u16, pub required:bool, pub max_bytes:u16,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Method {
    pub opcode:u16,pub major:u16,pub minor:u16,
    pub fields:&'static [Descriptor],
}
pub fn decode_fields<'a>(body:&'a [u8],schema:&[Descriptor])
    ->Result<Vec<Field<'a>>,Error> {
    if body.len()>65535 || schema.len()>64 {return Err(Error::Capacity)}
    let mut out=Vec::new();
    out.try_reserve_exact(schema.len()).map_err(|_|Error::Allocation)?;
    let mut cursor=0usize;
    let mut prior=0u16;
    while cursor<body.len() {
        if body.len()-cursor<4{return Err(Error::Short)}
        let tag=u16::from_le_bytes([body[cursor],body[cursor+1]]);
        let size=u16::from_le_bytes([body[cursor+2],body[cursor+3]]) as usize;
        cursor=cursor.checked_add(4).ok_or(Error::Overflow)?;
        let end=cursor.checked_add(size).ok_or(Error::Overflow)?;
        if end>body.len(){return Err(Error::Short)}
        if tag<=prior || tag==0{return Err(Error::Noncanonical)}
        let d=schema.iter().find(|d|d.tag==tag).ok_or(Error::UnknownField)?;
        if size>d.max_bytes as usize{return Err(Error::Capacity)}
        out.push(Field{tag,value:&body[cursor..end]});
        prior=tag;cursor=end;
    }
    for d in schema {
        if d.tag==0 || schema.iter().filter(|x|x.tag==d.tag).count()!=1 {
            return Err(Error::DuplicateField);
        }
        if d.required && !out.iter().any(|x|x.tag==d.tag) {
            return Err(Error::RequiredField)
        }
    }
    Ok(out)
}
pub fn encode_fields(fields:&[Field<'_>],schema:&[Descriptor])
    ->Result<Vec<u8>,Error> {
    if fields.len()>schema.len() || schema.len()>64{return Err(Error::Capacity)}
    let mut prior=0u16;
    let mut total=0usize;
    for f in fields {
        if f.tag<=prior || f.tag==0{return Err(Error::Noncanonical)}
        let d=schema.iter().find(|d|d.tag==f.tag).ok_or(Error::UnknownField)?;
        if f.value.len()>d.max_bytes as usize || f.value.len()>u16::MAX as usize {
            return Err(Error::Capacity);
        }
        total=total.checked_add(4).and_then(|v|v.checked_add(f.value.len()))
            .ok_or(Error::Overflow)?;
        prior=f.tag;
    }
    if total>65535{return Err(Error::Capacity)}
    for d in schema {
        if d.required && !fields.iter().any(|f|f.tag==d.tag) {
            return Err(Error::RequiredField)
        }
    }
    let mut bytes=Vec::new();
    bytes.try_reserve_exact(total).map_err(|_|Error::Allocation)?;
    for f in fields {
        bytes.extend_from_slice(&f.tag.to_le_bytes());
        bytes.extend_from_slice(&(f.value.len() as u16).to_le_bytes());
        bytes.extend_from_slice(f.value);
    }
    Ok(bytes)
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct RequestStamp {
    pub request_id:u64,pub caller_pid:u64,pub lease_epoch:u64,
    pub broker_epoch:u64,pub schema_opcode:u16,pub major:u16,pub minor:u16,
}
impl RequestStamp {
    pub fn valid(self)->bool {
        self.request_id!=0 && self.caller_pid!=0 && self.lease_epoch!=0
            && self.broker_epoch!=0 && self.schema_opcode!=0 && self.major!=0
    }
}
#[derive(Clone,Debug,Eq,PartialEq)]
pub struct Frame<'a>{
    pub stamp:RequestStamp,pub fields:Vec<Field<'a>>,
}
pub fn parse<'a>(bytes:&'a [u8],stamp:RequestStamp,method:&Method,
                 expected_pid:u64,lease:u64,broker:u64)
     ->Result<Frame<'a>,Error> {
    if !stamp.valid() || stamp.caller_pid!=expected_pid
        || stamp.lease_epoch!=lease{return Err(Error::WrongPrincipal)}
    if stamp.broker_epoch!=broker{return Err(Error::StaleEpoch)}
    if stamp.schema_opcode!=method.opcode{return Err(Error::WrongOperation)}
    if stamp.major!=method.major || stamp.minor>method.minor {
        return Err(Error::WrongVersion)
    }
    Ok(Frame{stamp,fields:decode_fields(bytes,method.fields)?})
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct SeenRequest {
    pub stamp:RequestStamp,pub payload_fingerprint:u64,pub committed:bool,
}
/// Fixed bounded replay gate. A request ID is never a capability; a matching
/// replay only answers from a committed authoritative result, never executes
/// the mutation again. The authoritative result remains external.
pub struct ReplayWindow<const N:usize>{
    recent:[Option<SeenRequest>;N], head:usize,
    last_epoch:u64,
}
impl<const N:usize> ReplayWindow<N> {
    pub const fn new()->Self{
        Self{recent:[None;N],head:0,last_epoch:0}
    }
    pub fn admit(&self,stamp:RequestStamp,fingerprint:u64)
        ->Result<Option<bool>,Error> {
        if !stamp.valid(){return Err(Error::WrongRequest)}
        if stamp.broker_epoch<self.last_epoch{return Err(Error::StaleEpoch)}
        for e in self.recent.iter().flatten() {
            if e.stamp.caller_pid==stamp.caller_pid
               && e.stamp.lease_epoch==stamp.lease_epoch
               && e.stamp.broker_epoch==stamp.broker_epoch
               && e.stamp.request_id==stamp.request_id {
                if e.stamp!=stamp || e.payload_fingerprint!=fingerprint {
                    return Err(Error::Replay);
                }
                return Ok(Some(e.committed))
            }
        }
        Ok(None)
    }
    pub fn remember(&mut self,entry:SeenRequest)->Result<(),Error>{
        if N==0 || !entry.stamp.valid(){return Err(Error::Capacity)}
        if self.admit(entry.stamp,entry.payload_fingerprint)?.is_some(){
            return Err(Error::Replay)
        }
        if entry.stamp.broker_epoch>self.last_epoch {
            if self.recent.iter().flatten().any(|e|!e.committed) {
                return Err(Error::Replay)
            }
            self.recent=[None;N];self.head=0;
            self.last_epoch=entry.stamp.broker_epoch;
        }
        if self.recent[self.head].is_some_and(|e|!e.committed) {
            return Err(Error::Capacity)
        }
        self.recent[self.head]=Some(entry);
        self.head=(self.head+1)%N;
        Ok(())
    }
    pub fn settle(&mut self,stamp:RequestStamp)->Result<(),Error> {
        let e=self.recent.iter_mut().flatten().find(|e|e.stamp==stamp)
            .ok_or(Error::WrongRequest)?;
        e.committed=true;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    static SCHEMA:[Descriptor;2]=[
        Descriptor{tag:1,required:true,max_bytes:8},
        Descriptor{tag:2,required:false,max_bytes:10},
    ];
    #[test]
    fn rejects_duplicate_unsorted_and_missing_required() {
        assert_eq!(decode_fields(&[2,0,0,0],&SCHEMA),Err(Error::RequiredField));
        assert_eq!(decode_fields(&[1,0,0,0,1,0,0,0],&SCHEMA),Err(Error::Noncanonical));
        assert!(decode_fields(&[1,0,1,0,9],&SCHEMA).is_ok());
    }
}
