//! Canonical, versioned Ed25519 release manifest and byte-for-byte image verification.
//!
//! This crate validates cryptographic evidence. It does not choose trusted
//! signers, distribute trust anchors, pin previous images, revoke kernel
//! handles, commit durable media or publish a new process. Every trusted key
//! must come from independently authenticated platform configuration.
//! Never treat a path, a claimed digest or a caller-supplied "verified" bit
//! as authority.
#![no_std]
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION:u16=1;
pub const MANIFEST_BYTES:usize=128;
pub const SIGNATURE_BYTES:usize=64;
pub const ENVELOPE_BYTES:usize=MANIFEST_BYTES+SIGNATURE_BYTES;
pub const MAX_IMAGE_BYTES:u64=128*1024*1024;
const MAGIC:[u8;4]=*b"RREL";
const DOMAIN:&[u8]=b"RustOS signed service image v1\0";

#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Length,Magic,Version,Flags,Reserved,ZeroIdentity,
    EmptyImage,TooLarge,WrongService,WrongSigner,UntrustedKey,
    RevokedKey,Generation,RollbackFloor,PreviousImage,
    ImageHash,InvalidKey,InvalidSignature,DuplicateSigner,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Manifest {
    pub service:u64,
    pub generation:u64,
    pub minimum_floor:u64,
    pub image_len:u64,
    pub image_sha256:[u8;32],
    pub signer_id:[u8;16],
    pub previous_sha256:[u8;32],
}
impl Manifest {
    pub fn valid(self)->bool{
        self.service!=0&&self.generation!=0
            &&self.image_len!=0&&self.image_len<=MAX_IMAGE_BYTES
            &&self.image_sha256.iter().any(|b|*b!=0)
            &&self.signer_id.iter().any(|b|*b!=0)
            &&self.previous_sha256.iter().any(|b|*b!=0)
            &&self.generation>=self.minimum_floor
    }
}
pub fn parse(bytes:&[u8])->Result<Manifest,Error>{
    if bytes.len()!=MANIFEST_BYTES{return Err(Error::Length)}
    if bytes[0..4]!=MAGIC{return Err(Error::Magic)}
    if u16::from_le_bytes([bytes[4],bytes[5]])!=FORMAT_VERSION{
        return Err(Error::Version)
    }
    if bytes[6]!=0||bytes[7]!=0{return Err(Error::Flags)}
    if bytes[120..128].iter().any(|b|*b!=0){return Err(Error::Reserved)}
    let u64at=|i:usize|{
        let mut value=[0u8;8];
        value.copy_from_slice(&bytes[i..i+8]);
        u64::from_le_bytes(value)
    };
    let mut digest=[0u8;32];
    let mut signer_id=[0u8;16];
    let mut previous=[0u8;32];
    digest.copy_from_slice(&bytes[40..72]);
    signer_id.copy_from_slice(&bytes[72..88]);
    previous.copy_from_slice(&bytes[88..120]);
    let m=Manifest{
        service:u64at(8),generation:u64at(16),
        minimum_floor:u64at(24),image_len:u64at(32),
        image_sha256:digest,signer_id,
        previous_sha256:previous,
    };
    if !m.valid(){return Err(Error::ZeroIdentity)}
    Ok(m)
}
/// Pure canonical encoding. The signature MUST be computed independently
/// over the domain-bound 128-byte manifest, never over a caller's pathname.
pub fn encode(m:Manifest)->Result<[u8;MANIFEST_BYTES],Error>{
    if !m.valid(){return Err(Error::ZeroIdentity)}
    let mut b=[0u8;MANIFEST_BYTES];
    b[..4].copy_from_slice(&MAGIC);
    b[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    b[8..16].copy_from_slice(&m.service.to_le_bytes());
    b[16..24].copy_from_slice(&m.generation.to_le_bytes());
    b[24..32].copy_from_slice(&m.minimum_floor.to_le_bytes());
    b[32..40].copy_from_slice(&m.image_len.to_le_bytes());
    b[40..72].copy_from_slice(&m.image_sha256);
    b[72..88].copy_from_slice(&m.signer_id);
    b[88..120].copy_from_slice(&m.previous_sha256);
    Ok(b)
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct TrustedKey{
    /// The signer ID is a registry key, NOT an identity proven by itself.
    pub signer_id:[u8;16],
    pub service:u64,
    pub verifying_key:[u8;32],
    pub minimum_generation:u64,
    pub revoked:bool,
}
pub struct KeyRing<'a> {
    entries:&'a [TrustedKey],
}
impl<'a> KeyRing<'a>{
    /// Duplicate signer IDs are forbidden across services too; otherwise
    /// inconsistent key rotation can silently choose the wrong authority.
    pub fn new(entries:&'a [TrustedKey])->Result<Self,Error>{
        for (i,e) in entries.iter().enumerate(){
            if e.service==0||e.signer_id==[0;16]
                ||e.verifying_key==[0;32]{return Err(Error::UntrustedKey)}
            if entries[..i].iter().any(|p|p.signer_id==e.signer_id){
                return Err(Error::DuplicateSigner)
            }
        }
        Ok(Self{entries})
    }
    fn locate(&self,signer:&[u8;16],service:u64)
        ->Result<&TrustedKey,Error>{
        let entry=self.entries.iter().find(|key|&key.signer_id==signer)
            .ok_or(Error::UntrustedKey)?;
        if entry.service!=service{return Err(Error::WrongSigner)}
        if entry.revoked{return Err(Error::RevokedKey)}
        Ok(entry)
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Verified {
    manifest:Manifest,
    image_sha256:[u8;32],
}
impl Verified{
    pub fn manifest(&self)->Manifest{self.manifest}
    pub fn digest(&self)->[u8;32]{self.image_sha256}
}
fn checksum(bytes:&[u8])->[u8;32]{
    let digest=Sha256::digest(bytes);
    let mut output=[0u8;32];
    output.copy_from_slice(&digest);
    output
}
/// Cryptographic verification of *both* candidate bytes and the exact
/// previous-image byte snapshot. The caller must independently prove both
/// byte streams are immutable/sealed and the previous image remains pinned
/// on stable storage before allowing a service switch.
pub fn verify_release(
    ring:&KeyRing<'_>, signed:&[u8],
    candidate:&[u8], previous:&[u8],
    expected_service:u64,durable_generation_floor:u64,
)->Result<Verified,Error>{
    if signed.len()!=ENVELOPE_BYTES{return Err(Error::Length)}
    let m=parse(&signed[..MANIFEST_BYTES])?;
    if m.service!=expected_service{return Err(Error::WrongService)}
    if m.generation<=durable_generation_floor
        ||m.minimum_floor<durable_generation_floor{
        return Err(Error::RollbackFloor)
    }
    if candidate.is_empty()||previous.is_empty(){return Err(Error::EmptyImage)}
    if candidate.len() as u64!=m.image_len{return Err(Error::Length)}
    if candidate.len() as u64>MAX_IMAGE_BYTES
        ||previous.len() as u64>MAX_IMAGE_BYTES{return Err(Error::TooLarge)}
    let key=ring.locate(&m.signer_id,m.service)?;
    if m.generation<key.minimum_generation{return Err(Error::Generation)}
    let verifying=VerifyingKey::from_bytes(&key.verifying_key)
        .map_err(|_|Error::InvalidKey)?;
    let mut raw_signature=[0u8;SIGNATURE_BYTES];
    raw_signature.copy_from_slice(&signed[MANIFEST_BYTES..]);
    let signature=Signature::from_bytes(&raw_signature);
    // Domain-separate release policy from all other Ed25519 uses. The
    // canonical bytes include the previous-image hash and anti-rollback floor.
    let mut message=[0u8;DOMAIN.len()+MANIFEST_BYTES];
    message[..DOMAIN.len()].copy_from_slice(DOMAIN);
    message[DOMAIN.len()..].copy_from_slice(&signed[..MANIFEST_BYTES]);
    verifying.verify_strict(&message,&signature)
        .map_err(|_|Error::InvalidSignature)?;
    if checksum(candidate)!=m.image_sha256{return Err(Error::ImageHash)}
    if checksum(previous)!=m.previous_sha256{return Err(Error::PreviousImage)}
    Ok(Verified{manifest:m,image_sha256:m.image_sha256})
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn canonical_manifest_roundtrip_rejects_reserved_and_bad_version(){
        let m=Manifest{service:3,generation:7,minimum_floor:5,
            image_len:22,image_sha256:[1;32],signer_id:[2;16],
            previous_sha256:[3;32]};
        let b=encode(m).unwrap();assert_eq!(parse(&b),Ok(m));
        let mut bad=b;bad[120]=1;
        assert_eq!(parse(&bad),Err(Error::Reserved));
        bad=b;bad[4]=42;
        assert_eq!(parse(&bad),Err(Error::Version));
    }
    #[test]
    fn rejects_unsigned_and_unknown_sentinel_keys(){
        let m=Manifest{service:3,generation:7,minimum_floor:5,
            image_len:22,image_sha256:[1;32],signer_id:[2;16],
            previous_sha256:[3;32]};
        let manifest=encode(m).unwrap();
        let mut signed=[0u8;ENVELOPE_BYTES];
        signed[..MANIFEST_BYTES].copy_from_slice(&manifest);
        let ring=KeyRing::new(&[]).unwrap();
        assert_eq!(verify_release(&ring,&signed,&[1;22],&[2;4],3,5),
            Err(Error::UntrustedKey));
        assert_eq!(verify_release(&ring,&signed,&[1;22],&[2;4],3,7),
            Err(Error::RollbackFloor));
    }
    #[test]
    fn signed_manifest_authenticates_candidate_and_old_bytes(){
        use ed25519_dalek::{Signer,SigningKey};
        let seed=[17u8;32];
        let signing=SigningKey::from_bytes(&seed);
        let id=[11u8;16];
        let candidate=b"trusted service image";
        let previous=b"pinned previous image";
        let floor=4;
        let manifest=Manifest {
            service:3,generation:floor+1,minimum_floor:floor,
            image_len:candidate.len() as u64,
            image_sha256:checksum(candidate),signer_id:id,
            previous_sha256:checksum(previous),
        };
        let bytes=encode(manifest).unwrap();
        let mut message=[0u8;DOMAIN.len()+MANIFEST_BYTES];
        message[..DOMAIN.len()].copy_from_slice(DOMAIN);
        message[DOMAIN.len()..].copy_from_slice(&bytes);
        let signature=signing.sign(&message).to_bytes();
        let mut signed=[0u8;ENVELOPE_BYTES];
        signed[..MANIFEST_BYTES].copy_from_slice(&bytes);
        signed[MANIFEST_BYTES..].copy_from_slice(&signature);
        let keys=[TrustedKey {
            signer_id:id,service:3,verifying_key:signing.verifying_key().to_bytes(),
            minimum_generation:4,revoked:false,
        }];
        let ring=KeyRing::new(&keys).unwrap();
        assert_eq!(verify_release(&ring,&signed,candidate,previous,3,floor)
            .unwrap().manifest(),manifest);
        assert_eq!(verify_release(&ring,&signed,b"mismatch",previous,3,floor),
            Err(Error::Length));
        assert_eq!(verify_release(&ring,&signed,candidate,b"wrong old image",3,floor),
            Err(Error::PreviousImage));
        assert_eq!(verify_release(&ring,&signed,candidate,previous,4,floor),
            Err(Error::WrongService));
        let mut tampered=signed;
        tampered[MANIFEST_BYTES+7]^=1;
        assert_eq!(verify_release(&ring,&tampered,candidate,previous,3,floor),
            Err(Error::InvalidSignature));
        let revoked=[TrustedKey{revoked:true,..keys[0]}];
        assert_eq!(verify_release(&KeyRing::new(&revoked).unwrap(),
            &signed,candidate,previous,3,floor),Err(Error::RevokedKey));
    }

}
