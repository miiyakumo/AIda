//! 仅支持 Linux、相对 descriptor 操作的持久化 content-addressed Artifact Store。

#![allow(
    clippy::missing_errors_doc,
    reason = "Rustdoc 依照仓库语言规范使用中文“错误”标题"
)]

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(test)]
use std::{cell::Cell, thread_local};

use rand::RngCore;
use rustix::fs::{
    AtFlags, CWD, Dir, Mode, OFlags, fstat, fsync, linkat, mkdirat, openat, statat, unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::{
    ArtifactAvailability, ArtifactHash, ArtifactRecord, DomainError, DurabilityCapability,
};

const LAYOUT_VERSION: u32 = 1;
const LAYOUT: &str = "artifacts-v1";
const MANIFEST: &str = "store-manifest-v1.json";
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONTROL_TRANSACTION_BYTES: usize = 256;
const DIRECTORY_MODE: Mode = Mode::from_raw_mode(0o700);
const FILE_MODE: Mode = Mode::from_raw_mode(0o600);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedArtifact {
    pub hash: ArtifactHash,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBlob {
    pub hash: ArtifactHash,
    pub size: u64,
}

pub struct VerifiedBlobFile {
    file: File,
    verified: VerifiedBlob,
}

impl VerifiedBlobFile {
    #[must_use]
    pub fn verified(&self) -> &VerifiedBlob {
        &self.verified
    }
}

impl Read for VerifiedBlobFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

/// 仅在 durable Store 成功提交后铸造的不透明证明。
///
/// 该类型有意不实现 `Clone` 或 `Deserialize`。
#[derive(Debug)]
pub struct CommittedArtifactReceipt {
    hash: ArtifactHash,
    size: u64,
    layout_version: u32,
    store_instance_id: String,
    durability: DurabilityCapability,
    commit_identity: String,
}

impl CommittedArtifactReceipt {
    #[cfg(test)]
    pub(crate) fn from_test_store(hash: ArtifactHash, size: u64) -> Self {
        let store_instance_id = "00000000000000000000000000000000".to_owned();
        let canonical = format!(
            "alda-artifact-commit-v1\n{}\n{size}\n{LAYOUT_VERSION}\n{store_instance_id}\nlinux_file_and_directory_synced\n",
            hash.as_str()
        );
        Self {
            hash,
            size,
            layout_version: LAYOUT_VERSION,
            store_instance_id,
            durability: DurabilityCapability::LinuxFileAndDirectorySynced,
            commit_identity: format!("sha256:{:x}", Sha256::digest(canonical.as_bytes())),
        }
    }

    #[must_use]
    pub fn hash(&self) -> &ArtifactHash {
        &self.hash
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn layout_version(&self) -> u32 {
        self.layout_version
    }

    #[must_use]
    pub fn store_instance_id(&self) -> &str {
        &self.store_instance_id
    }

    #[must_use]
    pub const fn durability(&self) -> DurabilityCapability {
        self.durability
    }

    #[must_use]
    pub fn commit_identity(&self) -> &str {
        &self.commit_identity
    }

    /// 冻结 control transaction 所需的 primitive audit fact。
    ///
    /// 返回值不含 live receipt capability，可安全写入 control WAL。
    #[allow(
        dead_code,
        reason = "B4a freezes this prerequisite before the B4 control WAL calls it"
    )]
    pub(crate) fn recovery_audit_plan(
        &self,
        control_transaction_id: impl Into<String>,
    ) -> Result<ArtifactAuditPlanV1, StoreError> {
        ArtifactAuditPlanV1::from_receipt(control_transaction_id.into(), self)
    }

    pub(crate) fn into_record(self) -> Result<ArtifactRecord, DomainError> {
        ArtifactRecord::verified_durable(
            self.hash,
            self.size,
            self.layout_version,
            self.store_instance_id,
            self.durability,
            self.commit_identity,
        )
    }
}

/// control WAL 为 Artifact redo 持久化的带版本 primitive fact。
///
/// 这里有意保存数据而非 capability。反序列化不会授权 `ArtifactRegistered` 事实；
/// 仍须经过 live Store audit。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactAuditPlanV1 {
    hash: String,
    size: u64,
    #[serde(rename = "layout")]
    layout_version: u32,
    #[serde(rename = "store_instance")]
    store_instance_id: String,
    durability: String,
    commit_identity: String,
    #[serde(rename = "control_tx")]
    control_transaction_id: String,
}

/// 已提交 Project 记录与 Prepared audit plan 全字段一致的不可变事实。
///
/// 该值不持有 Store、文件描述符或写入能力，只能由本模块的逐字段 matcher 构造。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedOccurrenceFact {
    hash: ArtifactHash,
    size: u64,
    layout_version: u32,
    store_instance_id: String,
    durability: DurabilityCapability,
    commit_identity: String,
    control_transaction_id: String,
}

#[allow(
    dead_code,
    reason = "C1b3 primitive 先冻结只读字段，后续 occurrence 叶子才会逐字段消费"
)]
impl CommittedOccurrenceFact {
    #[must_use]
    pub(crate) fn hash(&self) -> &ArtifactHash {
        &self.hash
    }

    #[must_use]
    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub(crate) const fn layout_version(&self) -> u32 {
        self.layout_version
    }

    #[must_use]
    pub(crate) fn store_instance_id(&self) -> &str {
        &self.store_instance_id
    }

    #[must_use]
    pub(crate) const fn durability(&self) -> DurabilityCapability {
        self.durability
    }

    #[must_use]
    pub(crate) fn commit_identity(&self) -> &str {
        &self.commit_identity
    }

    #[must_use]
    pub(crate) fn control_transaction_id(&self) -> &str {
        &self.control_transaction_id
    }
}

impl ArtifactAuditPlanV1 {
    #[allow(
        dead_code,
        reason = "B4a freezes this prerequisite before the B4 control WAL calls it"
    )]
    fn from_receipt(
        control_transaction_id: String,
        receipt: &CommittedArtifactReceipt,
    ) -> Result<Self, StoreError> {
        let plan = Self {
            hash: receipt.hash.as_str().to_owned(),
            size: receipt.size,
            layout_version: receipt.layout_version,
            store_instance_id: receipt.store_instance_id.clone(),
            durability: durability_name(receipt.durability).to_owned(),
            commit_identity: receipt.commit_identity.clone(),
            control_transaction_id,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn validate(&self) -> Result<ArtifactHash, StoreError> {
        validate_control_transaction_id(&self.control_transaction_id)?;
        if self.size > MAX_ARTIFACT_BYTES
            || self.layout_version != LAYOUT_VERSION
            || self.store_instance_id.len() != 32
            || !is_lower_hex(&self.store_instance_id)
            || self.durability != durability_name(DurabilityCapability::LinuxFileAndDirectorySynced)
        {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        let hash = ArtifactHash::parse(self.hash.clone())
            .map_err(|_| StoreError::RecoveryAuditMismatch)?;
        let expected_identity = artifact_commit_identity(
            &hash,
            self.size,
            self.layout_version,
            &self.store_instance_id,
            &self.durability,
        );
        if self.commit_identity != expected_identity {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        Ok(hash)
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "B4a freezes this prerequisite before the B4 control codec calls it"
    )]
    pub(crate) fn control_transaction_id(&self) -> &str {
        &self.control_transaction_id
    }

    pub(crate) fn validate_for_control(
        &self,
        expected_control_transaction_id: &str,
    ) -> Result<(), StoreError> {
        validate_control_transaction_id(expected_control_transaction_id)?;
        if self.control_transaction_id != expected_control_transaction_id {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        self.validate().map(|_| ())
    }

    /// 比较 audit plan 与 stored Project record，并返回纯数据证明。
    pub(crate) fn match_committed_record(
        &self,
        expected_control_transaction_id: &str,
        stored_record: &ArtifactRecord,
    ) -> Result<CommittedOccurrenceFact, StoreError> {
        self.validate_for_control(expected_control_transaction_id)?;
        stored_record
            .validate_audit()
            .map_err(|_| StoreError::RecoveryAuditMismatch)?;
        if stored_record.availability() != ArtifactAvailability::VerifiedDurable
            || stored_record.hash().as_str() != self.hash
            || stored_record.size() != self.size
            || stored_record.layout_version() != Some(self.layout_version)
            || stored_record.store_instance_id() != Some(self.store_instance_id.as_str())
            || stored_record.durability() != Some(DurabilityCapability::LinuxFileAndDirectorySynced)
            || self.durability != durability_name(DurabilityCapability::LinuxFileAndDirectorySynced)
            || stored_record.store_commit_identity() != Some(self.commit_identity.as_str())
        {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        Ok(CommittedOccurrenceFact {
            hash: stored_record.hash().clone(),
            size: self.size,
            layout_version: self.layout_version,
            store_instance_id: self.store_instance_id.clone(),
            durability: DurabilityCapability::LinuxFileAndDirectorySynced,
            commit_identity: self.commit_identity.clone(),
            control_transaction_id: self.control_transaction_id.clone(),
        })
    }
}

/// 每次打开时独立生成且不可伪造的 authority，只能由 durable runtime Store 构造器
/// 或本模块测试签发。
///
/// 该类型有意不实现 `Clone` 或 serialization trait。
#[derive(Debug)]
pub(crate) struct ArtifactRecoveryGuard {
    authority: Arc<ArtifactRecoveryAuthority>,
}

#[derive(Debug)]
struct ArtifactRecoveryAuthority;

/// 指定持久化 audit plan 已重新验证的一次性证明。
///
/// 该类型有意不实现 `Clone` 或 serialization trait；唯一会消费它的交接入口保持
/// crate-private，并由可信 Project stored-plan converter 使用。
#[derive(Debug)]
pub(crate) struct RecoveredArtifactCapability {
    audited_plan: ArtifactAuditPlanV1,
}

impl RecoveredArtifactCapability {
    pub(crate) fn into_project_record(
        self,
        expected_plan: &ArtifactAuditPlanV1,
    ) -> Result<ArtifactRecord, StoreError> {
        if &self.audited_plan != expected_plan {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        let hash = self.audited_plan.validate()?;
        ArtifactRecord::verified_durable(
            hash,
            self.audited_plan.size,
            self.audited_plan.layout_version,
            self.audited_plan.store_instance_id,
            DurabilityCapability::LinuxFileAndDirectorySynced,
            self.audited_plan.commit_identity,
        )
        .map_err(|_| StoreError::RecoveryAuditMismatch)
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Artifact Store root is invalid")]
    InvalidRoot,
    #[error("Artifact Store path contains a symlink or unsafe component")]
    UnsafeSymlink,
    #[error("required filesystem safety is unsupported")]
    UnsupportedSafety,
    #[error("invalid Artifact hash")]
    InvalidHash,
    #[error("Artifact exceeds the 64 MiB limit")]
    TooLarge,
    #[error("expected Artifact hash does not match")]
    ExpectedHashMismatch,
    #[error("expected Artifact size does not match")]
    ExpectedSizeMismatch,
    #[error("filesystem operation failed: {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("an existing blob is corrupt")]
    ExistingBlobCorrupt,
    #[error("blob was not found")]
    BlobNotFound,
    #[error("blob verification failed")]
    BlobCorrupt,
    #[error("required durability is unsupported")]
    UnsupportedDurability,
    #[error("Artifact recovery guard does not belong to this Store handle")]
    RecoveryGuardMismatch,
    #[error("Artifact recovery audit plan does not match trusted facts")]
    RecoveryAuditMismatch,
    #[error("the primary operation failed and staging cleanup also failed")]
    CleanupFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestBody {
    schema_version: u32,
    layout_version: u32,
    store_instance_id: String,
    durability: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoreManifest {
    body: ManifestBody,
    checksum: String,
}

pub struct ArtifactStore {
    _root: OwnedFd,
    _layout: OwnedFd,
    staging: OwnedFd,
    sha256: OwnedFd,
    pins: OwnedFd,
    instance_id: String,
    recovery_authority: Arc<ArtifactRecoveryAuthority>,
    #[cfg(test)]
    failpoint: AtomicU8,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum StoreFailpoint {
    AfterTempCreate = 1,
    AfterTempWrite = 2,
    BeforeTempSync = 3,
    AfterTempSync = 4,
    AfterTempVerify = 5,
    BeforeBlobInstall = 6,
    BeforeWinnerVerify = 7,
    AfterBlobInstall = 8,
    BeforeShardSync = 9,
    BeforeStagingUnlink = 10,
    BeforeStagingSync = 11,
    CleanupFailure = 12,
    AfterPinWrite = 13,
    BeforePinInstall = 14,
    BeforePinsSync = 15,
    BeforePinCleanup = 16,
    BeforePinStagingSync = 17,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum OpenFailpoint {
    AfterLayoutSync = 1,
    AfterStagingSync = 2,
    AfterManifestFileSync = 3,
    BeforeManifestInstall = 4,
    BeforeManifestLayoutSync = 5,
    AfterManifestInstall = 6,
    AfterBlobsSync = 7,
    AfterSha256Sync = 8,
    AfterPinsSync = 9,
}

#[cfg(test)]
thread_local! {
    static OPEN_FAILPOINT: Cell<u8> = const { Cell::new(0) };
}

impl ArtifactStore {
    /// 安全地打开或初始化 Linux Artifact Store。
    ///
    /// # 错误
    ///
    /// root 不安全、manifest 无效，或发生 I/O 与持久性失败时返回类型化错误。
    #[cfg(target_os = "linux")]
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let root = open_absolute_directory(root)?;
        validate_owned_private_directory(&root, true)?;
        let layout = ensure_directory(&root, LAYOUT)?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterLayoutSync)?;
        let staging = ensure_directory(&layout, "staging")?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterStagingSync)?;
        let manifest = load_or_create_manifest(&layout, &staging)?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterManifestInstall)?;
        let blobs = ensure_directory(&layout, "blobs")?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterBlobsSync)?;
        let sha256 = ensure_directory(&blobs, "sha256")?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterSha256Sync)?;
        let pins = ensure_directory(&layout, "pins")?;
        #[cfg(test)]
        fail_open_if(OpenFailpoint::AfterPinsSync)?;
        fsync(&layout).map_err(|source| io_error("sync layout directory", source))?;
        Ok(Self {
            _root: root,
            _layout: layout,
            staging,
            sha256,
            pins,
            instance_id: manifest.body.store_instance_id,
            recovery_authority: Arc::new(ArtifactRecoveryAuthority),
            #[cfg(test)]
            failpoint: AtomicU8::new(0),
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open(_root: &Path) -> Result<Self, StoreError> {
        Err(StoreError::UnsupportedSafety)
    }

    /// 打开 Store，同时返回 durable runtime 恢复所需的不可伪造 authority。
    ///
    /// 该构造器保持 crate-private，防止 wire、plugin 与普通公开 Store 调用者取得恢复铸造权。
    #[allow(
        dead_code,
        reason = "B4a freezes this prerequisite before the B4 composition root calls it"
    )]
    pub(crate) fn open_for_durable_runtime(
        root: &Path,
    ) -> Result<(Self, ArtifactRecoveryGuard), StoreError> {
        let store = Self::open(root)?;
        let guard = ArtifactRecoveryGuard {
            authority: Arc::clone(&store.recovery_authority),
        };
        Ok((store, guard))
    }

    #[cfg(test)]
    fn open_with_failpoint(root: &Path, failpoint: OpenFailpoint) -> Result<Self, StoreError> {
        OPEN_FAILPOINT.with(|value| value.set(failpoint as u8));
        let result = Self::open(root);
        OPEN_FAILPOINT.with(|value| value.set(0));
        result
    }

    /// 流式读取、校验并持久提交一个 Artifact。
    ///
    /// # 错误
    ///
    /// 只有完整的文件与目录同步策略成功后才返回 receipt。
    pub fn put(
        &self,
        mut reader: impl Read,
        expected: Option<&ExpectedArtifact>,
    ) -> Result<CommittedArtifactReceipt, StoreError> {
        let temp_name = random_name("blob", "tmp");
        let fd = openat(
            &self.staging,
            temp_name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(|source| io_error("create staging file", source))?;
        let mut temp = File::from(fd);
        #[cfg(test)]
        if let Err(error) = self.fail_if(StoreFailpoint::AfterTempCreate) {
            let _ignored = unlinkat(&self.staging, temp_name.as_str(), AtFlags::empty());
            let _ignored = fsync(&self.staging);
            return Err(error);
        }
        #[cfg(test)]
        if self.fail_if(StoreFailpoint::CleanupFailure).is_err() {
            return Err(StoreError::CleanupFailed);
        }
        let result = self.put_from_temp(&mut reader, &mut temp, &temp_name, expected);
        if result.is_err() {
            let _ignored = unlinkat(&self.staging, temp_name.as_str(), AtFlags::empty());
            let _ignored = fsync(&self.staging);
        }
        result
    }

    fn put_from_temp(
        &self,
        reader: &mut impl Read,
        temp: &mut File,
        temp_name: &str,
        expected: Option<&ExpectedArtifact>,
    ) -> Result<CommittedArtifactReceipt, StoreError> {
        let (hash, size) = stream_copy_hash(reader, temp)?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::AfterTempWrite)?;
        temp.flush()
            .map_err(|source| io_error("flush staging file", source))?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforeTempSync)?;
        temp.sync_all()
            .map_err(|source| io_error("sync staging file", source))?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::AfterTempSync)?;
        let verified = verify_file(temp, StoreError::BlobCorrupt)?;
        if verified.hash != hash || verified.size != size {
            return Err(StoreError::BlobCorrupt);
        }
        #[cfg(test)]
        self.fail_if(StoreFailpoint::AfterTempVerify)?;
        if let Some(expected) = expected {
            if expected.hash != hash {
                return Err(StoreError::ExpectedHashMismatch);
            }
            if expected.size != size {
                return Err(StoreError::ExpectedSizeMismatch);
            }
        }
        let hex = hash_hex(&hash)?;
        let shard = ensure_directory(&self.sha256, &hex[..2])?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforeBlobInstall)?;
        match linkat(
            &self.staging,
            temp_name,
            &shard,
            hex.as_str(),
            AtFlags::empty(),
        ) {
            Ok(()) => {
                temp.sync_all()
                    .map_err(|source| io_error("sync installed blob", source))?;
                #[cfg(test)]
                self.fail_if(StoreFailpoint::AfterBlobInstall)?;
            }
            Err(rustix::io::Errno::EXIST) => {
                #[cfg(test)]
                self.fail_if(StoreFailpoint::BeforeWinnerVerify)?;
                let winner = open_blob_file(&shard, &hex, StoreError::ExistingBlobCorrupt)?;
                let winner = verify_owned_file(winner, StoreError::ExistingBlobCorrupt)?;
                if winner.hash != hash || winner.size != size {
                    return Err(StoreError::ExistingBlobCorrupt);
                }
            }
            Err(source) => return Err(io_error("install blob without replacement", source)),
        }
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforeShardSync)?;
        fsync(&shard).map_err(|source| io_error("sync blob shard", source))?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforeStagingUnlink)?;
        unlinkat(&self.staging, temp_name, AtFlags::empty())
            .map_err(|source| io_error("unlink staging file", source))?;
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforeStagingSync)?;
        fsync(&self.staging).map_err(|source| io_error("sync staging directory", source))?;
        Ok(self.receipt(hash, size))
    }

    /// 通过相对 descriptor 且 no-follow 的打开方式验证已安装 blob。
    ///
    /// # 错误
    ///
    /// hash 路径不存在、不安全、不可读、过大或内容不匹配请求 hash 时返回类型化错误。
    pub fn verify(&self, hash: &ArtifactHash) -> Result<VerifiedBlob, StoreError> {
        let file = self.open_blob(hash)?;
        let verified = verify_owned_file(file, StoreError::BlobCorrupt)?;
        if &verified.hash != hash {
            return Err(StoreError::BlobCorrupt);
        }
        Ok(verified)
    }

    /// 验证并回卷同一 handle，再返回该 handle 供读取。
    ///
    /// # 错误
    ///
    /// 返回与 [`Self::verify`] 相同的验证或 I/O 错误。
    pub fn get(&self, hash: &ArtifactHash) -> Result<VerifiedBlobFile, StoreError> {
        let mut file = self.open_blob(hash)?;
        let verified = verify_file(&mut file, StoreError::BlobCorrupt)?;
        if &verified.hash != hash {
            return Err(StoreError::BlobCorrupt);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|source| io_error("rewind verified blob", source))?;
        Ok(VerifiedBlobFile { file, verified })
    }

    /// 重新验证持久化 audit plan，并铸造一项可消费的 Project 恢复 capability。
    ///
    /// 调用者必须先通过 Project transaction probe 证明对应 aggregate transaction 为
    /// `Absent`。重试时只有再次探测到 `Absent` 才能调用；若结果为 `SamePlanCommitted`，
    /// 则完全跳过铸造。
    pub(crate) fn audit_recovery_artifact(
        &self,
        guard: &ArtifactRecoveryGuard,
        expected_control_transaction_id: &str,
        plan: &ArtifactAuditPlanV1,
    ) -> Result<RecoveredArtifactCapability, StoreError> {
        if !Arc::ptr_eq(&self.recovery_authority, &guard.authority) {
            return Err(StoreError::RecoveryGuardMismatch);
        }
        validate_control_transaction_id(expected_control_transaction_id)?;
        if plan.control_transaction_id != expected_control_transaction_id {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        let hash = plan.validate()?;
        if plan.store_instance_id != self.instance_id
            || plan.layout_version != LAYOUT_VERSION
            || plan.durability != durability_name(DurabilityCapability::LinuxFileAndDirectorySynced)
        {
            return Err(StoreError::RecoveryAuditMismatch);
        }

        // `get` 对实际打开的 descriptor 计算 hash 并回卷；audit 从同一 live handle
        // 读取验证事实，不会在校验后重新打开路径。
        let opened = self.get(&hash)?;
        if opened.verified().hash != hash || opened.verified().size != plan.size {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        let expected_identity = artifact_commit_identity(
            &hash,
            plan.size,
            LAYOUT_VERSION,
            &self.instance_id,
            durability_name(DurabilityCapability::LinuxFileAndDirectorySynced),
        );
        if expected_identity != plan.commit_identity {
            return Err(StoreError::RecoveryAuditMismatch);
        }
        Ok(RecoveredArtifactCapability {
            audited_plan: plan.clone(),
        })
    }

    /// 验证 blob 后创建幂等、持久化的 pin marker。
    ///
    /// # 错误
    ///
    /// 返回类型化验证或持久性错误，但不移除被引用的 blob。
    pub fn pin(&self, hash: &ArtifactHash) -> Result<(), StoreError> {
        self.verify(hash)?;
        let hex = hash_hex(hash)?;
        let temp_name = random_name("pin", "tmp");
        let fd = openat(
            &self.staging,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        )
        .map_err(|source| io_error("create pin staging file", source))?;
        let mut file = File::from(fd);
        file.write_all(format!("sha256:{hex}\n").as_bytes())
            .map_err(|source| io_error("write pin marker", source))?;
        #[cfg(test)]
        if let Err(error) = self.fail_if(StoreFailpoint::AfterPinWrite) {
            let _ignored = unlinkat(&self.staging, temp_name.as_str(), AtFlags::empty());
            let _ignored = fsync(&self.staging);
            return Err(error);
        }
        file.sync_all()
            .map_err(|source| io_error("sync pin marker", source))?;
        let marker_name = format!("{hex}.pin");
        let expected_contents = format!("sha256:{hex}\n");
        let result = (|| {
            #[cfg(test)]
            self.fail_if(StoreFailpoint::BeforePinInstall)?;
            match linkat(
                &self.staging,
                temp_name.as_str(),
                &self.pins,
                marker_name.as_str(),
                AtFlags::empty(),
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {
                    let fd = openat(
                        &self.pins,
                        marker_name.as_str(),
                        FILE_READ_FLAGS | OFlags::NONBLOCK,
                        Mode::empty(),
                    )
                    .map_err(|source| match source {
                        rustix::io::Errno::LOOP => StoreError::BlobCorrupt,
                        _ => io_error("open existing pin marker", source),
                    })?;
                    let existing = File::from(fd);
                    let metadata = existing
                        .metadata()
                        .map_err(|source| io_error("inspect existing pin marker", source))?;
                    let expected_len = u64::try_from(expected_contents.len())
                        .map_err(|_| StoreError::BlobCorrupt)?;
                    if !metadata.file_type().is_file()
                        || metadata.uid() != rustix::process::getuid().as_raw()
                        || metadata.mode() & 0o077 != 0
                        || metadata.len() != expected_len
                    {
                        return Err(StoreError::BlobCorrupt);
                    }
                    let mut contents = Vec::new();
                    existing
                        .take(expected_len + 1)
                        .read_to_end(&mut contents)
                        .map_err(|source| io_error("read existing pin marker", source))?;
                    if contents != expected_contents.as_bytes() {
                        return Err(StoreError::BlobCorrupt);
                    }
                }
                Err(source) => return Err(io_error("install pin marker", source)),
            }
            #[cfg(test)]
            self.fail_if(StoreFailpoint::BeforePinsSync)?;
            fsync(&self.pins).map_err(|source| io_error("sync pins directory", source))
        })();
        #[cfg(test)]
        self.fail_if(StoreFailpoint::BeforePinCleanup)?;
        let cleanup = unlinkat(&self.staging, temp_name.as_str(), AtFlags::empty())
            .map_err(|source| io_error("unlink pin staging file", source));
        let cleanup = cleanup.and_then(|()| {
            #[cfg(test)]
            self.fail_if(StoreFailpoint::BeforePinStagingSync)?;
            fsync(&self.staging).map_err(|source| io_error("sync staging directory", source))
        });
        result.and(cleanup)
    }

    /// 列出有效的普通 staging entry，但不删除它们。
    ///
    /// # 错误
    ///
    /// 无法读取已锚定 staging 目录时返回 I/O 错误。
    pub fn list_staging_orphans(&self) -> Result<Vec<String>, StoreError> {
        list_regular_names(&self.staging, |name| {
            Path::new(name)
                .extension()
                .is_some_and(|value| value == "tmp")
        })
    }

    /// 列出所有有效的已安装 blob hash。
    ///
    /// # 错误
    ///
    /// 无法打开或读取已锚定目录时返回类型化错误。
    pub fn list_blob_hashes(&self) -> Result<Vec<ArtifactHash>, StoreError> {
        let mut hashes = Vec::new();
        for shard_name in list_directory_names(&self.sha256)? {
            if shard_name.len() != 2 || !is_lower_hex(&shard_name) {
                continue;
            }
            let shard = open_directory(&self.sha256, &shard_name)?;
            for name in list_regular_names(&shard, |candidate| {
                candidate.len() == 64
                    && is_lower_hex(candidate)
                    && candidate.starts_with(&shard_name)
            })? {
                hashes.push(
                    ArtifactHash::parse(format!("sha256:{name}"))
                        .map_err(|_| StoreError::InvalidHash)?,
                );
            }
        }
        hashes.sort();
        Ok(hashes)
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn open_blob(&self, hash: &ArtifactHash) -> Result<File, StoreError> {
        let hex = hash_hex(hash)?;
        let shard = match open_directory(&self.sha256, &hex[..2]) {
            Ok(shard) => shard,
            Err(StoreError::Io { source, .. })
                if source.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) =>
            {
                return Err(StoreError::BlobNotFound);
            }
            Err(error) => return Err(error),
        };
        open_blob_file(&shard, &hex, StoreError::BlobCorrupt)
    }

    fn receipt(&self, hash: ArtifactHash, size: u64) -> CommittedArtifactReceipt {
        let durability = DurabilityCapability::LinuxFileAndDirectorySynced;
        let commit_identity = artifact_commit_identity(
            &hash,
            size,
            LAYOUT_VERSION,
            &self.instance_id,
            durability_name(durability),
        );
        CommittedArtifactReceipt {
            hash,
            size,
            layout_version: LAYOUT_VERSION,
            store_instance_id: self.instance_id.clone(),
            durability,
            commit_identity,
        }
    }

    #[cfg(test)]
    fn set_failpoint(&self, failpoint: StoreFailpoint) {
        self.failpoint.store(failpoint as u8, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn fail_if(&self, failpoint: StoreFailpoint) -> Result<(), StoreError> {
        if self
            .failpoint
            .compare_exchange(failpoint as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(io_error(
                "injected Artifact Store failure",
                std::io::Error::other("test failpoint"),
            ));
        }
        Ok(())
    }
}

fn durability_name(capability: DurabilityCapability) -> &'static str {
    match capability {
        DurabilityCapability::LinuxFileAndDirectorySynced => "linux_file_and_directory_synced",
    }
}

fn artifact_commit_identity(
    hash: &ArtifactHash,
    size: u64,
    layout_version: u32,
    store_instance_id: &str,
    durability: &str,
) -> String {
    let canonical = format!(
        "alda-artifact-commit-v1\n{}\n{size}\n{layout_version}\n{store_instance_id}\n{durability}\n",
        hash.as_str()
    );
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn validate_control_transaction_id(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > MAX_CONTROL_TRANSACTION_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(StoreError::RecoveryAuditMismatch);
    }
    Ok(())
}

fn open_absolute_directory(path: &Path) -> Result<OwnedFd, StoreError> {
    if !path.is_absolute() {
        return Err(StoreError::InvalidRoot);
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes[1..]
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(StoreError::InvalidRoot);
    }
    let mut current = openat(CWD, "/", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|source| io_error("open trusted filesystem root", source))?;
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) if !name.is_empty() => {
                saw_component = true;
                current =
                    openat(&current, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|source| {
                        match source {
                            rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
                                StoreError::UnsafeSymlink
                            }
                            _ => io_error("open root component", source),
                        }
                    })?;
            }
            _ => return Err(StoreError::InvalidRoot),
        }
    }
    if !saw_component {
        return Err(StoreError::InvalidRoot);
    }
    Ok(current)
}

#[cfg(test)]
fn fail_open_if(failpoint: OpenFailpoint) -> Result<(), StoreError> {
    let matched = OPEN_FAILPOINT.with(|value| {
        if value.get() == failpoint as u8 {
            value.set(0);
            true
        } else {
            false
        }
    });
    if matched {
        return Err(io_error(
            "injected Artifact Store open failure",
            std::io::Error::other("test open failpoint"),
        ));
    }
    Ok(())
}

fn validate_owned_private_directory(fd: &OwnedFd, root: bool) -> Result<(), StoreError> {
    let stat = fstat(fd).map_err(|source| io_error("inspect directory", source))?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || if root {
            stat.st_mode & 0o022 != 0
        } else {
            stat.st_mode & 0o077 != 0
        }
    {
        return Err(StoreError::InvalidRoot);
    }
    Ok(())
}

fn ensure_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, StoreError> {
    match mkdirat(parent, name, DIRECTORY_MODE) {
        Ok(()) => {
            let child = open_directory(parent, name)?;
            fsync(&child).map_err(|source| io_error("sync new directory", source))?;
            fsync(parent).map_err(|source| io_error("sync parent directory", source))?;
            Ok(child)
        }
        Err(rustix::io::Errno::EXIST) => open_directory(parent, name),
        Err(source) => Err(io_error("create directory", source)),
    }
}

fn open_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, StoreError> {
    let fd =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|source| match source {
            rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => StoreError::UnsafeSymlink,
            _ => io_error("open directory", source),
        })?;
    validate_owned_private_directory(&fd, false)?;
    Ok(fd)
}

fn load_or_create_manifest(
    layout: &OwnedFd,
    staging: &OwnedFd,
) -> Result<StoreManifest, StoreError> {
    match openat(layout, MANIFEST, FILE_READ_FLAGS, Mode::empty()) {
        Ok(fd) => read_manifest(File::from(fd)),
        Err(rustix::io::Errno::NOENT) => {
            let body = ManifestBody {
                schema_version: 1,
                layout_version: LAYOUT_VERSION,
                store_instance_id: random_hex_128(),
                durability: "linux_file_and_directory_synced".to_owned(),
            };
            let checksum = manifest_checksum(&body)?;
            let manifest = StoreManifest { body, checksum };
            let bytes =
                serde_json::to_vec(&manifest).map_err(|_| StoreError::UnsupportedDurability)?;
            let temp_name = random_name("manifest", "tmp");
            let fd = openat(
                staging,
                temp_name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                FILE_MODE,
            )
            .map_err(|source| io_error("create manifest staging file", source))?;
            let mut file = File::from(fd);
            file.write_all(&bytes)
                .map_err(|source| io_error("write manifest", source))?;
            file.sync_all()
                .map_err(|source| io_error("sync manifest", source))?;
            #[cfg(test)]
            fail_open_if(OpenFailpoint::AfterManifestFileSync)?;
            file.seek(SeekFrom::Start(0))
                .map_err(|source| io_error("rewind manifest", source))?;
            let verified = read_manifest(file)?;
            #[cfg(test)]
            fail_open_if(OpenFailpoint::BeforeManifestInstall)?;
            match linkat(
                staging,
                temp_name.as_str(),
                layout,
                MANIFEST,
                AtFlags::empty(),
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {
                    let winner = openat(layout, MANIFEST, FILE_READ_FLAGS, Mode::empty())
                        .map_err(|source| io_error("open manifest winner", source))?;
                    let winner = read_manifest(File::from(winner))?;
                    unlinkat(staging, temp_name.as_str(), AtFlags::empty())
                        .map_err(|source| io_error("unlink manifest staging file", source))?;
                    fsync(staging).map_err(|source| io_error("sync staging directory", source))?;
                    return Ok(winner);
                }
                Err(source) => return Err(io_error("install manifest", source)),
            }
            #[cfg(test)]
            fail_open_if(OpenFailpoint::BeforeManifestLayoutSync)?;
            fsync(layout).map_err(|source| io_error("sync manifest directory", source))?;
            unlinkat(staging, temp_name.as_str(), AtFlags::empty())
                .map_err(|source| io_error("unlink manifest staging file", source))?;
            fsync(staging).map_err(|source| io_error("sync staging directory", source))?;
            Ok(verified)
        }
        Err(rustix::io::Errno::LOOP) => Err(StoreError::UnsafeSymlink),
        Err(source) => Err(io_error("open manifest", source)),
    }
}

fn read_manifest(mut file: File) -> Result<StoreManifest, StoreError> {
    let stat = file
        .metadata()
        .map_err(|source| io_error("inspect manifest", source))?;
    if !stat.file_type().is_file()
        || stat.len() > 4096
        || stat.uid() != rustix::process::getuid().as_raw()
        || stat.mode() & 0o077 != 0
    {
        return Err(StoreError::UnsupportedDurability);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| io_error("read manifest", source))?;
    let manifest: StoreManifest =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::UnsupportedDurability)?;
    if manifest.body.schema_version != 1
        || manifest.body.layout_version != LAYOUT_VERSION
        || manifest.body.durability != "linux_file_and_directory_synced"
        || manifest.checksum != manifest_checksum(&manifest.body)?
        || manifest.body.store_instance_id.len() != 32
        || !is_lower_hex(&manifest.body.store_instance_id)
    {
        return Err(StoreError::UnsupportedDurability);
    }
    Ok(manifest)
}

fn manifest_checksum(body: &ManifestBody) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(&("alda-store-manifest-v1", body))
        .map_err(|_| StoreError::UnsupportedDurability)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn stream_copy_hash(
    reader: &mut impl Read,
    writer: &mut File,
) -> Result<(ArtifactHash, u64), StoreError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| io_error("read Artifact input", source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).map_err(|_| StoreError::TooLarge)?)
            .ok_or(StoreError::TooLarge)?;
        if size > MAX_ARTIFACT_BYTES {
            return Err(StoreError::TooLarge);
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|source| io_error("write staging file", source))?;
        hasher.update(&buffer[..count]);
    }
    ArtifactHash::parse(format!("sha256:{:x}", hasher.finalize()))
        .map(|hash| (hash, size))
        .map_err(|_| StoreError::InvalidHash)
}

fn verify_owned_file(mut file: File, corrupt: StoreError) -> Result<VerifiedBlob, StoreError> {
    verify_file(&mut file, corrupt)
}

fn verify_file(file: &mut File, corrupt: StoreError) -> Result<VerifiedBlob, StoreError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect blob", source))?;
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_ARTIFACT_BYTES
    {
        return Err(corrupt);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("rewind blob", source))?;
    let (hash, size) = stream_read_hash(file)?;
    Ok(VerifiedBlob { hash, size })
}

fn stream_read_hash(reader: &mut impl Read) -> Result<(ArtifactHash, u64), StoreError> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| io_error("read blob", source))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).map_err(|_| StoreError::BlobCorrupt)?)
            .ok_or(StoreError::BlobCorrupt)?;
        if size > MAX_ARTIFACT_BYTES {
            return Err(StoreError::BlobCorrupt);
        }
        hasher.update(&buffer[..count]);
    }
    let hash = ArtifactHash::parse(format!("sha256:{:x}", hasher.finalize()))
        .map_err(|_| StoreError::InvalidHash)?;
    Ok((hash, size))
}

fn open_blob_file(shard: &OwnedFd, hex: &str, corrupt: StoreError) -> Result<File, StoreError> {
    match openat(shard, hex, FILE_READ_FLAGS, Mode::empty()) {
        Ok(fd) => {
            let file = File::from(fd);
            if !file
                .metadata()
                .map_err(|source| io_error("inspect blob", source))?
                .file_type()
                .is_file()
            {
                return Err(corrupt);
            }
            Ok(file)
        }
        Err(rustix::io::Errno::NOENT) => Err(StoreError::BlobNotFound),
        Err(rustix::io::Errno::LOOP) => Err(corrupt),
        Err(source) => Err(io_error("open blob", source)),
    }
}

fn list_regular_names(
    directory: &OwnedFd,
    predicate: impl Fn(&str) -> bool,
) -> Result<Vec<String>, StoreError> {
    let mut names = Vec::new();
    let mut entries =
        Dir::read_from(directory).map_err(|source| io_error("open directory stream", source))?;
    for entry in &mut entries {
        let entry = entry.map_err(|source| io_error("read directory entry", source))?;
        let name = entry.file_name().to_string_lossy();
        if name == "." || name == ".." || !predicate(&name) {
            continue;
        }
        let stat = statat(directory, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| io_error("inspect directory entry", source))?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
            names.push(name.into_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn list_directory_names(directory: &OwnedFd) -> Result<Vec<String>, StoreError> {
    let mut names = Vec::new();
    let mut entries =
        Dir::read_from(directory).map_err(|source| io_error("open directory stream", source))?;
    for entry in &mut entries {
        let entry = entry.map_err(|source| io_error("read directory entry", source))?;
        let name = entry.file_name().to_string_lossy();
        if name == "." || name == ".." {
            continue;
        }
        let stat = statat(directory, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| io_error("inspect directory entry", source))?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir() {
            names.push(name.into_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn hash_hex(hash: &ArtifactHash) -> Result<String, StoreError> {
    let value = hash.as_str();
    value
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && is_lower_hex(hex))
        .map(ToOwned::to_owned)
        .ok_or(StoreError::InvalidHash)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn random_hex_128() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut value, byte| {
            write!(value, "{byte:02x}").expect("writing to String cannot fail");
            value
        })
}

fn random_name(prefix: &str, suffix: &str) -> String {
    format!("{prefix}-{}.{suffix}", random_hex_128())
}

fn io_error(operation: &'static str, source: impl Into<std::io::Error>) -> StoreError {
    StoreError::Io {
        operation,
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;

    use super::*;

    const FIXTURE_HASH: &str =
        "sha256:f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d";

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("fixture read failure"))
        }
    }

    fn store() -> (tempfile::TempDir, ArtifactStore) {
        let root = tempfile::tempdir().expect("temporary root");
        let store = ArtifactStore::open(root.path()).expect("open store");
        (root, store)
    }

    fn runtime_store() -> (tempfile::TempDir, ArtifactStore, ArtifactRecoveryGuard) {
        let root = tempfile::tempdir().expect("temporary runtime root");
        let (store, guard) =
            ArtifactStore::open_for_durable_runtime(root.path()).expect("open runtime store");
        (root, store, guard)
    }

    fn assert_preinstall_failure(failpoint: StoreFailpoint) {
        let (_root, store) = store();
        store.set_failpoint(failpoint);
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), None),
            Err(StoreError::Io { .. })
        ));
        assert!(store.list_staging_orphans().expect("orphans").is_empty());
        assert!(matches!(
            store.verify(&ArtifactHash::parse(FIXTURE_HASH).expect("hash")),
            Err(StoreError::BlobNotFound)
        ));
    }

    fn assert_postinstall_failure(failpoint: StoreFailpoint) {
        let (_root, store) = store();
        store.set_failpoint(failpoint);
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), None),
            Err(StoreError::Io { .. })
        ));
        assert_eq!(
            store
                .verify(&ArtifactHash::parse(FIXTURE_HASH).expect("hash"))
                .expect("installed orphan remains verifiable")
                .size,
            7
        );
    }

    #[test]
    fn fixed_put_verify_get_receipt_reopen_pin_and_lists() {
        let (root, store) = store();
        let expected = ExpectedArtifact {
            hash: ArtifactHash::parse(FIXTURE_HASH).expect("fixed hash"),
            size: 7,
        };
        let receipt = store
            .put(Cursor::new(b"fixture"), Some(&expected))
            .expect("put fixture");
        assert_eq!(receipt.hash().as_str(), FIXTURE_HASH);
        assert_eq!(receipt.size(), 7);
        assert_eq!(receipt.layout_version(), 1);
        assert_eq!(
            receipt.durability(),
            DurabilityCapability::LinuxFileAndDirectorySynced
        );
        let canonical = format!(
            "alda-artifact-commit-v1\n{FIXTURE_HASH}\n7\n1\n{}\nlinux_file_and_directory_synced\n",
            receipt.store_instance_id()
        );
        assert_eq!(
            receipt.commit_identity(),
            format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
        );
        assert_eq!(store.verify(receipt.hash()).expect("verify").size, 7);
        let mut opened = store.get(receipt.hash()).expect("get");
        let mut bytes = Vec::new();
        opened
            .read_to_end(&mut bytes)
            .expect("read verified handle");
        assert_eq!(bytes, b"fixture");

        let duplicate = store
            .put(Cursor::new(b"fixture"), None)
            .expect("deduplicated put");
        assert_eq!(duplicate.hash(), receipt.hash());
        assert_eq!(
            store.list_blob_hashes().expect("list blobs"),
            vec![receipt.hash().clone()]
        );
        store.pin(receipt.hash()).expect("pin");
        store.pin(receipt.hash()).expect("idempotent pin");
        assert!(store.list_staging_orphans().expect("orphans").is_empty());

        let instance = store.instance_id().to_owned();
        drop(store);
        assert_eq!(
            ArtifactStore::open(root.path())
                .expect("reopen")
                .instance_id(),
            instance
        );
    }

    #[test]
    fn expected_mismatch_too_large_and_missing_leave_no_receipt_or_temp() {
        let (_root, store) = store();
        let wrong_hash = ExpectedArtifact {
            hash: ArtifactHash::parse(format!("sha256:{}", "a".repeat(64))).expect("hash"),
            size: 7,
        };
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), Some(&wrong_hash)),
            Err(StoreError::ExpectedHashMismatch)
        ));
        let wrong_size = ExpectedArtifact {
            hash: ArtifactHash::parse(FIXTURE_HASH).expect("hash"),
            size: 8,
        };
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), Some(&wrong_size)),
            Err(StoreError::ExpectedSizeMismatch)
        ));
        assert!(matches!(
            store.put(std::io::repeat(0).take(MAX_ARTIFACT_BYTES + 1), None),
            Err(StoreError::TooLarge)
        ));
        assert!(matches!(
            store.put(FailingReader, None),
            Err(StoreError::Io {
                operation: "read Artifact input",
                ..
            })
        ));
        assert!(store.list_staging_orphans().expect("orphans").is_empty());
        assert!(matches!(
            store.verify(&ArtifactHash::parse(FIXTURE_HASH).expect("hash")),
            Err(StoreError::BlobNotFound)
        ));
    }

    #[test]
    fn put_and_pin_failpoints_never_mint_error_receipts_or_leak_staging_files() {
        for failpoint in [
            StoreFailpoint::AfterTempCreate,
            StoreFailpoint::AfterTempWrite,
            StoreFailpoint::BeforeTempSync,
            StoreFailpoint::AfterTempSync,
            StoreFailpoint::AfterTempVerify,
            StoreFailpoint::BeforeBlobInstall,
        ] {
            assert_preinstall_failure(failpoint);
        }
        for failpoint in [
            StoreFailpoint::AfterBlobInstall,
            StoreFailpoint::BeforeShardSync,
            StoreFailpoint::BeforeStagingUnlink,
            StoreFailpoint::BeforeStagingSync,
        ] {
            assert_postinstall_failure(failpoint);
        }
        let (_root, store) = store();
        let hash = store
            .put(Cursor::new(b"fixture"), None)
            .expect("blob for pin failures")
            .hash()
            .clone();
        for failpoint in [
            StoreFailpoint::AfterPinWrite,
            StoreFailpoint::BeforePinInstall,
            StoreFailpoint::BeforePinsSync,
            StoreFailpoint::BeforePinStagingSync,
        ] {
            store.set_failpoint(failpoint);
            assert!(matches!(store.pin(&hash), Err(StoreError::Io { .. })));
        }
    }

    #[test]
    fn winner_and_cleanup_failpoints_preserve_authoritative_objects() {
        {
            let (_root, store) = store();
            let hash = store
                .put(Cursor::new(b"fixture"), None)
                .expect("winner")
                .hash()
                .clone();
            store.set_failpoint(StoreFailpoint::BeforeWinnerVerify);
            assert!(matches!(
                store.put(Cursor::new(b"fixture"), None),
                Err(StoreError::Io { .. })
            ));
            assert_eq!(store.verify(&hash).expect("winner intact").size, 7);
        }

        let (_root, store) = store();
        store.set_failpoint(StoreFailpoint::CleanupFailure);
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), None),
            Err(StoreError::CleanupFailed)
        ));
        assert_eq!(store.list_staging_orphans().expect("orphan").len(), 1);

        let pin_root = tempfile::tempdir().expect("pin failpoint root");
        let pin_store = ArtifactStore::open(pin_root.path()).expect("pin failpoint store");
        let hash = pin_store
            .put(Cursor::new(b"fixture"), None)
            .expect("blob")
            .hash()
            .clone();
        pin_store.set_failpoint(StoreFailpoint::BeforePinCleanup);
        assert!(matches!(pin_store.pin(&hash), Err(StoreError::Io { .. })));
        assert_eq!(
            pin_store.list_staging_orphans().expect("pin orphan").len(),
            1
        );
    }

    #[test]
    fn initialization_failpoints_leave_a_reopenable_or_explicitly_incomplete_store() {
        for failpoint in [
            OpenFailpoint::AfterLayoutSync,
            OpenFailpoint::AfterStagingSync,
            OpenFailpoint::AfterManifestFileSync,
            OpenFailpoint::BeforeManifestInstall,
            OpenFailpoint::BeforeManifestLayoutSync,
            OpenFailpoint::AfterManifestInstall,
            OpenFailpoint::AfterBlobsSync,
            OpenFailpoint::AfterSha256Sync,
            OpenFailpoint::AfterPinsSync,
        ] {
            let root = tempfile::tempdir().expect("temporary root");
            assert!(matches!(
                ArtifactStore::open_with_failpoint(root.path(), failpoint),
                Err(StoreError::Io { .. })
            ));
            let reopened = ArtifactStore::open(root.path()).expect("recover initialization");
            assert_eq!(reopened.list_blob_hashes().expect("empty blobs"), vec![]);
            assert!(reopened.list_staging_orphans().expect("staging").len() <= 1);
        }
    }

    #[test]
    fn existing_pin_special_files_and_unbounded_or_weak_markers_fail_closed() {
        let (root, store) = store();
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("blob");
        let hex = hash_hex(receipt.hash()).expect("hex");
        let marker = format!("{hex}.pin");

        rustix::fs::mkfifoat(&store.pins, marker.as_str(), FILE_MODE).expect("fifo marker");
        let store = Arc::new(store);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let store = Arc::clone(&store);
            let hash = receipt.hash().clone();
            std::thread::spawn(move || tx.send(store.pin(&hash)).expect("send result"))
        };
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1))
                .expect("pin must not block"),
            Err(StoreError::BlobCorrupt)
        ));
        worker.join().expect("pin worker");
        unlinkat(&store.pins, marker.as_str(), AtFlags::empty()).expect("remove fifo");

        let marker_path = root.path().join(LAYOUT).join("pins").join(&marker);
        fs::write(&marker_path, vec![b'x'; 4096]).expect("oversized marker");
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600))
            .expect("private marker");
        assert!(matches!(
            store.pin(receipt.hash()),
            Err(StoreError::BlobCorrupt)
        ));
        fs::remove_file(&marker_path).expect("remove oversized marker");

        fs::write(&marker_path, format!("sha256:{hex}\n")).expect("weak-mode marker");
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644))
            .expect("weak marker mode");
        assert!(matches!(
            store.pin(receipt.hash()),
            Err(StoreError::BlobCorrupt)
        ));
    }

    #[test]
    fn concurrent_identical_puts_install_one_blob_without_replacement() {
        let (_root, store) = store();
        let store = Arc::new(store);
        let threads = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .put(Cursor::new(b"fixture"), None)
                        .expect("concurrent put")
                })
            })
            .collect::<Vec<_>>();
        let receipts = threads
            .into_iter()
            .map(|thread| thread.join().expect("thread"))
            .collect::<Vec<_>>();
        assert!(
            receipts
                .iter()
                .all(|receipt| receipt.hash() == receipts[0].hash())
        );
        assert_eq!(store.list_blob_hashes().expect("list").len(), 1);
    }

    #[test]
    fn symlink_components_and_existing_non_regular_blob_fail_closed() {
        let parent = tempfile::tempdir().expect("parent");
        let real = parent.path().join("real");
        fs::create_dir(&real).expect("real");
        let root = real.join("root");
        fs::create_dir(&root).expect("root");
        symlink(&real, parent.path().join("link")).expect("intermediate symlink");
        assert!(matches!(
            ArtifactStore::open(&parent.path().join("link/root")),
            Err(StoreError::UnsafeSymlink)
        ));

        let store = ArtifactStore::open(&root).expect("safe store");
        let hex = FIXTURE_HASH.strip_prefix("sha256:").expect("prefix");
        let shard = root.join(LAYOUT).join("blobs/sha256").join(&hex[..2]);
        fs::create_dir(&shard).expect("shard");
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o700)).expect("private shard");
        fs::create_dir(shard.join(hex)).expect("directory as blob");
        assert!(matches!(
            store.put(Cursor::new(b"fixture"), None),
            Err(StoreError::ExistingBlobCorrupt)
        ));
    }

    #[test]
    fn verified_get_keeps_same_inode_after_path_replacement() {
        let (root, store) = store();
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
        let mut opened = store.get(receipt.hash()).expect("verified handle");
        let hex = hash_hex(receipt.hash()).expect("hex");
        let path = root
            .path()
            .join(LAYOUT)
            .join("blobs/sha256")
            .join(&hex[..2])
            .join(&hex);
        fs::remove_file(&path).expect("remove final path");
        fs::write(&path, b"replacement").expect("replace final path");
        let mut bytes = Vec::new();
        opened
            .read_to_end(&mut bytes)
            .expect("read original handle");
        assert_eq!(bytes, b"fixture");
        assert!(matches!(
            store.verify(receipt.hash()),
            Err(StoreError::BlobCorrupt)
        ));
    }

    #[test]
    fn acquired_root_capability_survives_path_rename_without_escape() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("root");
        fs::create_dir(&root).expect("root");
        let store = ArtifactStore::open(&root).expect("store");
        let moved = parent.path().join("moved");
        fs::rename(&root, &moved).expect("rename root");
        fs::create_dir(&root).expect("replacement root");
        store
            .put(Cursor::new(b"fixture"), None)
            .expect("put via fd");
        assert!(moved.join(LAYOUT).join("blobs/sha256/f1").exists());
        assert!(!root.join(LAYOUT).exists());
    }

    #[test]
    fn lost_receipt_is_reaudited_after_reopen_with_canonical_identity() {
        let root = tempfile::tempdir().expect("root");
        let (store, guard) =
            ArtifactStore::open_for_durable_runtime(root.path()).expect("open runtime store");
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
        let plan = receipt
            .recovery_audit_plan("control-tx:artifact-1")
            .expect("freeze audit plan");
        let expected_hash = receipt.hash().clone();
        let expected_identity = receipt.commit_identity().to_owned();
        assert_eq!(
            serde_json::to_value(&plan).expect("primitive audit JSON"),
            serde_json::json!({
                "hash": expected_hash.as_str(),
                "size": 7,
                "layout": 1,
                "store_instance": receipt.store_instance_id(),
                "durability": "linux_file_and_directory_synced",
                "commit_identity": expected_identity,
                "control_tx": "control-tx:artifact-1"
            })
        );
        drop(receipt);
        drop(guard);
        drop(store);

        let (store, guard) =
            ArtifactStore::open_for_durable_runtime(root.path()).expect("reopen runtime store");
        let capability = store
            .audit_recovery_artifact(&guard, "control-tx:artifact-1", &plan)
            .expect("reaudit lost receipt");
        let recovered = capability
            .into_project_record(&plan)
            .expect("consume recovered capability");
        assert_eq!(recovered.hash(), &expected_hash);
        assert_eq!(recovered.size(), 7);
        assert_eq!(
            recovered.store_commit_identity(),
            Some(expected_identity.as_str())
        );
        assert_eq!(plan.control_transaction_id(), "control-tx:artifact-1");
    }

    #[test]
    fn recovery_audit_rejects_wrong_guard_store_control_transaction_and_plan() {
        let (_first_root, first_store, first_guard) = runtime_store();
        let receipt = first_store.put(Cursor::new(b"fixture"), None).expect("put");
        let plan = receipt
            .recovery_audit_plan("control-tx:artifact-2")
            .expect("plan");
        let (_second_root, second_store, second_guard) = runtime_store();

        assert!(matches!(
            first_store.audit_recovery_artifact(&second_guard, "control-tx:artifact-2", &plan),
            Err(StoreError::RecoveryGuardMismatch)
        ));
        assert!(matches!(
            second_store.audit_recovery_artifact(&second_guard, "control-tx:artifact-2", &plan),
            Err(StoreError::RecoveryAuditMismatch)
        ));
        assert!(matches!(
            first_store.audit_recovery_artifact(&first_guard, "control-tx:different", &plan),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_size = plan.clone();
        wrong_size.size += 1;
        assert!(matches!(
            first_store.audit_recovery_artifact(&first_guard, "control-tx:artifact-2", &wrong_size),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_identity = plan.clone();
        wrong_identity.commit_identity = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            first_store.audit_recovery_artifact(
                &first_guard,
                "control-tx:artifact-2",
                &wrong_identity
            ),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_instance = plan;
        wrong_instance.store_instance_id = "f".repeat(32);
        wrong_instance.commit_identity = artifact_commit_identity(
            receipt.hash(),
            receipt.size(),
            LAYOUT_VERSION,
            &wrong_instance.store_instance_id,
            durability_name(DurabilityCapability::LinuxFileAndDirectorySynced),
        );
        assert!(matches!(
            first_store.audit_recovery_artifact(
                &first_guard,
                "control-tx:artifact-2",
                &wrong_instance
            ),
            Err(StoreError::RecoveryAuditMismatch)
        ));
    }

    #[test]
    fn artifact_audit_match_requires_every_committed_field_and_control_identity() {
        let (_root, store, _guard) = runtime_store();
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
        let plan = receipt
            .recovery_audit_plan("control-tx:occurrence-match")
            .expect("audit plan");
        let record = receipt.into_record().expect("committed record");

        let fact = plan
            .match_committed_record("control-tx:occurrence-match", &record)
            .expect("全字段匹配");
        assert_eq!(fact.hash(), record.hash());
        assert_eq!(fact.size(), record.size());
        assert_eq!(
            fact.layout_version(),
            record.layout_version().expect("layout")
        );
        assert_eq!(
            fact.store_instance_id(),
            record.store_instance_id().expect("Store identity")
        );
        assert_eq!(fact.durability(), record.durability().expect("durability"));
        assert_eq!(
            fact.commit_identity(),
            record.store_commit_identity().expect("commit identity")
        );
        assert_eq!(fact.control_transaction_id(), "control-tx:occurrence-match");

        let rebind_identity = |candidate: &mut ArtifactAuditPlanV1| {
            let hash = ArtifactHash::parse(candidate.hash.clone()).expect("candidate hash");
            candidate.commit_identity = artifact_commit_identity(
                &hash,
                candidate.size,
                candidate.layout_version,
                &candidate.store_instance_id,
                &candidate.durability,
            );
        };

        let mut wrong_hash = plan.clone();
        wrong_hash.hash = format!("sha256:{}", "a".repeat(64));
        rebind_identity(&mut wrong_hash);
        assert!(matches!(
            wrong_hash.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_size = plan.clone();
        wrong_size.size += 1;
        rebind_identity(&mut wrong_size);
        assert!(matches!(
            wrong_size.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_layout = plan.clone();
        wrong_layout.layout_version += 1;
        rebind_identity(&mut wrong_layout);
        assert!(matches!(
            wrong_layout.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_store = plan.clone();
        wrong_store.store_instance_id = "f".repeat(32);
        rebind_identity(&mut wrong_store);
        assert!(matches!(
            wrong_store.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_durability = plan.clone();
        wrong_durability.durability = "file_only".to_owned();
        rebind_identity(&mut wrong_durability);
        assert!(matches!(
            wrong_durability.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));

        let mut wrong_commit = plan.clone();
        wrong_commit.commit_identity = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            wrong_commit.match_committed_record("control-tx:occurrence-match", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));
        assert!(matches!(
            plan.match_committed_record("control-tx:different", &record),
            Err(StoreError::RecoveryAuditMismatch)
        ));
    }

    #[test]
    fn recovery_audit_rejects_replaced_or_corrupt_blob() {
        for replacement_kind in ["truncate", "replace_inode"] {
            let (root, store, guard) = runtime_store();
            let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
            let plan = receipt
                .recovery_audit_plan("control-tx:artifact-3")
                .expect("plan");
            let hex = hash_hex(receipt.hash()).expect("hex");
            let blob_path = root
                .path()
                .join(LAYOUT)
                .join("blobs/sha256")
                .join(&hex[..2])
                .join(hex);
            if replacement_kind == "replace_inode" {
                fs::remove_file(&blob_path).expect("unlink original blob");
            }
            fs::write(&blob_path, b"replacement").expect("write corrupt replacement");
            fs::set_permissions(&blob_path, fs::Permissions::from_mode(0o600))
                .expect("private blob mode");

            assert!(matches!(
                store.audit_recovery_artifact(&guard, "control-tx:artifact-3", &plan),
                Err(StoreError::BlobCorrupt | StoreError::RecoveryAuditMismatch)
            ));
        }
    }
}
