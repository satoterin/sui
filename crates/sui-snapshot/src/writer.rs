// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

use crate::{
    compute_sha3_checksum, create_file_metadata, Blob, Encoding, FileCompression, FileMetadata,
    FileType, Manifest, ManifestV1, BLOB_ENCODING_BYTES, FILE_MAX_BYTES, MAGIC_BYTES,
    MANIFEST_FILE_MAGIC, OBJECT_FILE_MAGIC, OBJECT_REF_BYTES, REFERENCE_FILE_MAGIC,
    SEQUENCE_NUM_BYTES,
};
use anyhow::{anyhow, Context, Result};
use backoff::future::retry;
use byteorder::{BigEndian, ByteOrder};
use futures::StreamExt;
use integer_encoding::VarInt;
use object_store::path::Path;
use object_store::DynObjectStore;
use std::collections::hash_map::Entry::Vacant;
use std::collections::HashMap;
use std::fs;
use std::fs::{read_dir, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use sui_core::authority::authority_store_tables::{AuthorityPerpetualTables, LiveObject};
use sui_storage::object_store::util::{copy_file, delete_recursively, path_to_filesystem};
use sui_storage::object_store::ObjectStoreConfig;
use sui_types::base_types::{ObjectID, ObjectRef};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

/// LiveObjectSetWriterV1 writes live object set. It creates multiple *.obj files and one REFERENCE file
struct LiveObjectSetWriterV1 {
    dir_path: PathBuf,
    bucket_num: u32,
    current_part_num: u32,
    wbuf: BufWriter<File>,
    ref_wbuf: BufWriter<File>,
    n: usize,
    files: Vec<FileMetadata>,
    sender: Option<Sender<FileMetadata>>,
    file_compression: FileCompression,
}

impl LiveObjectSetWriterV1 {
    async fn new(
        dir_path: PathBuf,
        bucket_num: u32,
        file_compression: FileCompression,
        sender: Sender<FileMetadata>,
    ) -> Result<Self> {
        let (n, obj_file, part_num) = Self::next_object_file(dir_path.clone(), bucket_num)?;
        let ref_file = Self::ref_file(dir_path.clone(), bucket_num)?;
        Ok(LiveObjectSetWriterV1 {
            dir_path,
            bucket_num,
            current_part_num: part_num,
            wbuf: BufWriter::new(obj_file),
            ref_wbuf: BufWriter::new(ref_file),
            n,
            files: vec![],
            sender: Some(sender),
            file_compression,
        })
    }
    pub async fn write(&mut self, object: &LiveObject) -> Result<()> {
        let object_reference = object.object_reference();
        self.write_object(object).await?;
        self.write_object_ref(&object_reference).await?;
        Ok(())
    }
    pub async fn done(mut self) -> Result<Vec<FileMetadata>> {
        self.finalize().await?;
        self.finalize_ref().await?;
        self.sender = None;
        Ok(self.files.clone())
    }
    fn next_object_file(dir_path: PathBuf, bucket_num: u32) -> Result<(usize, File, u32)> {
        let part_num = Self::next_object_file_number(dir_path.clone(), bucket_num)?;
        let next_part_file_path = dir_path.join(format!("{bucket_num}_{part_num}.obj"));
        let next_part_file_tmp_path = dir_path.join(format!("{bucket_num}_{part_num}.obj.tmp"));
        let mut f = File::create(next_part_file_tmp_path.clone())?;
        let mut metab = [0u8; MAGIC_BYTES];
        BigEndian::write_u32(&mut metab, OBJECT_FILE_MAGIC);
        f.rewind()?;
        let n = f.write(&metab)?;
        drop(f);
        fs::rename(next_part_file_tmp_path, next_part_file_path.clone())?;
        let mut f = OpenOptions::new().append(true).open(next_part_file_path)?;
        f.seek(SeekFrom::Start(n as u64))?;
        Ok((n, f, part_num))
    }
    fn next_object_file_number(dir: PathBuf, bucket: u32) -> Result<u32> {
        let files = read_dir(&dir)?;
        let mut max = 0u32;
        for file_path in files {
            let entry = file_path?;
            let file_name = format!("{:?}", entry.file_name());
            if !file_name.ends_with(".obj") {
                continue;
            }
            let (bucket_num, part_num) = file_name
                .strip_suffix(".obj")
                .context(format!("Invalid object file {file_name} in snapshot dir {}, should be named <bucket_num>_<part_num>.obj", dir.display()))?
                .split_once('_')
                .map(|(b, p)| (b.parse::<u32>(), (p.parse::<u32>())))
                .ok_or(anyhow!("Failed to parse object file name: {file_name} in dir {}", dir.display()))?;
            if bucket_num? != bucket {
                continue;
            }
            let part_num = part_num?;
            if part_num > max {
                max = part_num;
            }
        }
        Ok(max + 1)
    }
    fn ref_file(dir_path: PathBuf, bucket_num: u32) -> Result<File> {
        let ref_path = dir_path.join(format!("REFERENCE-{bucket_num}"));
        let ref_tmp_path = dir_path.join(format!("REFERENCE-{bucket_num}.tmp"));
        let mut f = File::create(ref_tmp_path.clone())?;
        f.rewind()?;
        let mut metab = [0u8; MAGIC_BYTES];
        BigEndian::write_u32(&mut metab, REFERENCE_FILE_MAGIC);
        let n = f.write(&metab)?;
        drop(f);
        fs::rename(ref_tmp_path, ref_path.clone())?;
        let mut f = OpenOptions::new().append(true).open(ref_path)?;
        f.seek(SeekFrom::Start(n as u64))?;
        Ok(f)
    }
    async fn finalize(&mut self) -> Result<()> {
        self.wbuf.flush()?;
        self.wbuf.get_ref().sync_data()?;
        let off = self.wbuf.get_ref().stream_position()?;
        self.wbuf.get_ref().set_len(off)?;
        let file_path = self
            .dir_path
            .join(format!("{}_{}.obj", self.bucket_num, self.current_part_num));
        let file_metadata = create_file_metadata(
            &file_path,
            self.file_compression,
            FileType::Object,
            self.bucket_num,
            self.current_part_num,
        )?;
        self.files.push(file_metadata.clone());
        if let Some(sender) = &self.sender {
            sender.send(file_metadata).await?;
        }
        Ok(())
    }
    async fn finalize_ref(&mut self) -> Result<()> {
        self.ref_wbuf.flush()?;
        self.ref_wbuf.get_ref().sync_data()?;
        let off = self.ref_wbuf.get_ref().stream_position()?;
        self.ref_wbuf.get_ref().set_len(off)?;
        let file_path = self.dir_path.join(format!("REFERENCE-{}", self.bucket_num));
        let file_metadata = create_file_metadata(
            &file_path,
            self.file_compression,
            FileType::Reference,
            self.bucket_num,
            0,
        )?;
        self.files.push(file_metadata.clone());
        if let Some(sender) = &self.sender {
            sender.send(file_metadata).await?;
        }
        Ok(())
    }
    async fn cut(&mut self) -> Result<()> {
        self.finalize().await?;
        let delim = [0u8; OBJECT_REF_BYTES];
        self.ref_wbuf.write_all(&delim)?;
        let (n, f, part_num) = Self::next_object_file(self.dir_path.clone(), self.bucket_num)?;
        self.n = n;
        self.current_part_num = part_num;
        self.wbuf = BufWriter::new(f);
        Ok(())
    }
    async fn write_object(&mut self, object: &LiveObject) -> Result<()> {
        let blob = Blob::encode(object, Encoding::Bcs)?;
        let mut blob_size = blob.data.len().required_space();
        blob_size += BLOB_ENCODING_BYTES;
        blob_size += blob.data.len();
        let cut_new_part_file = (self.n + blob_size) > FILE_MAX_BYTES;
        if cut_new_part_file {
            self.cut().await?;
        }
        self.n += blob.append_to_file(&mut self.wbuf)?;
        Ok(())
    }
    async fn write_object_ref(&mut self, object_ref: &ObjectRef) -> Result<()> {
        let mut buf = [0u8; OBJECT_REF_BYTES];
        buf[0..ObjectID::LENGTH].copy_from_slice(object_ref.0.as_ref());
        BigEndian::write_u64(
            &mut buf[ObjectID::LENGTH..OBJECT_REF_BYTES],
            object_ref.1.value(),
        );
        buf[ObjectID::LENGTH + SEQUENCE_NUM_BYTES..OBJECT_REF_BYTES]
            .copy_from_slice(object_ref.2.as_ref());
        self.ref_wbuf.write_all(&buf)?;
        Ok(())
    }
}

/// StateSnapshotWriterV1 writes snapshot files to a local staging dir and simultaneously uploads them
/// to a remote object store
pub struct StateSnapshotWriterV1 {
    epoch: u64,
    local_staging_dir: File,
    local_staging_dir_root: PathBuf,
    file_compression: FileCompression,
    remote_object_store: Arc<DynObjectStore>,
    local_object_store: Arc<DynObjectStore>,
    concurrency: usize,
}

impl StateSnapshotWriterV1 {
    pub async fn new(
        epoch: u64,
        local_store_config: &ObjectStoreConfig,
        remote_store_config: &ObjectStoreConfig,
        file_compression: FileCompression,
        concurrency: NonZeroUsize,
    ) -> Result<Self> {
        let epoch_dir = format!("epoch_{epoch}");
        let remote_object_store = remote_store_config.make()?;
        // Delete remote epoch dir if it exists
        delete_recursively(
            &Path::from(epoch_dir.clone()),
            remote_object_store.clone(),
            concurrency,
        )
        .await?;

        let local_object_store = local_store_config.make()?;
        let local_staging_dir_root = local_store_config
            .directory
            .as_ref()
            .context("No local directory specified")?
            .clone();

        // Delete local epoch dir if it exists
        let local_epoch_dir_path = local_staging_dir_root.join(&epoch_dir);
        if local_epoch_dir_path.exists() {
            return Err(anyhow!(
                "Local epoch dir already exists: {:?}",
                local_epoch_dir_path
            ));
        }
        fs::create_dir_all(&local_epoch_dir_path)?;
        let local_staging_dir = File::open(&local_epoch_dir_path)?;
        Ok(StateSnapshotWriterV1 {
            epoch,
            local_staging_dir,
            local_staging_dir_root,
            file_compression,
            remote_object_store,
            local_object_store,
            concurrency: concurrency.get(),
        })
    }
    pub async fn write(mut self, perpetual_db: &AuthorityPerpetualTables) -> Result<()> {
        let (sender, receiver) = mpsc::channel::<FileMetadata>(1000);
        let upload_handle = self.start_upload(receiver)?;
        let files = self
            .write_live_object_set(perpetual_db, sender, Self::bucket_func)
            .await?;
        self.local_staging_dir.sync_data()?;
        self.write_manifest(files)?;
        upload_handle.await?.context(format!(
            "Failed to upload state snapshot for epoch: {}",
            &self.epoch
        ))?;
        // Upload MANIFEST in the very end
        let manifest_file_path = self.epoch_dir().child("MANIFEST");
        copy_file(
            manifest_file_path.clone(),
            manifest_file_path.clone(),
            self.local_object_store,
            self.remote_object_store,
        )
        .await?;
        Ok(())
    }
    fn start_upload(
        &self,
        receiver: Receiver<FileMetadata>,
    ) -> Result<JoinHandle<Result<Vec<()>, anyhow::Error>>> {
        let remote_object_store = self.remote_object_store.clone();
        let local_object_store = self.local_object_store.clone();
        let local_dir_path = self.local_staging_dir_root.clone();
        let epoch_dir = self.epoch_dir();
        let upload_concurrency = self.concurrency;
        let join_handle = tokio::spawn(async move {
            let results: Vec<Result<(), anyhow::Error>> = ReceiverStream::new(receiver)
                .map(|file_metadata| {
                    let backoff = backoff::ExponentialBackoff::default();
                    let file_path = file_metadata.file_path(&epoch_dir);
                    let remote_object_store = remote_object_store.clone();
                    let local_object_store = local_object_store.clone();
                    let local_dir_path = local_dir_path.clone();
                    async move {
                        retry(backoff, || async {
                            copy_file(
                                file_path.clone(),
                                file_path.clone(),
                                local_object_store.clone(),
                                remote_object_store.clone(),
                            )
                            .await
                            .map_err(|e| anyhow!("Failed to upload state snapshot file: {e}"))
                            .map_err(backoff::Error::transient)?;
                            // Delete file from local filesystem as soon as it is done uploading
                            let local_file_path =
                                path_to_filesystem(local_dir_path.clone(), &file_path.clone())?;
                            if local_file_path.exists() {
                                fs::remove_file(local_file_path)
                                    .map_err(|e| anyhow!("Failed to delete local file: {e}"))
                                    .map_err(backoff::Error::transient)?;
                            }
                            Ok(())
                        })
                        .await?;
                        Ok(())
                    }
                })
                .boxed()
                .buffer_unordered(upload_concurrency)
                .collect()
                .await;
            results
                .into_iter()
                .collect::<Result<Vec<()>, anyhow::Error>>()
        });
        Ok(join_handle)
    }
    async fn write_live_object_set<F>(
        &mut self,
        perpetual_db: &AuthorityPerpetualTables,
        sender: Sender<FileMetadata>,
        bucket_func: F,
    ) -> Result<Vec<FileMetadata>>
    where
        F: Fn(&LiveObject) -> u32,
    {
        let mut object_writers: HashMap<u32, LiveObjectSetWriterV1> = HashMap::new();
        let local_staging_dir_path =
            path_to_filesystem(self.local_staging_dir_root.clone(), &self.epoch_dir())?;
        for object in perpetual_db.iter_live_object_set() {
            let bucket_num = bucket_func(&object);
            if let Vacant(entry) = object_writers.entry(bucket_num) {
                entry.insert(
                    LiveObjectSetWriterV1::new(
                        local_staging_dir_path.clone(),
                        bucket_num,
                        self.file_compression,
                        sender.clone(),
                    )
                    .await?,
                );
            }
            let writer = object_writers
                .get_mut(&bucket_num)
                .context("Unexpected missing bucket writer")?;
            writer.write(&object).await?;
        }
        let mut files = vec![];
        for (_, writer) in object_writers.into_iter() {
            files.extend(writer.done().await?);
        }
        Ok(files)
    }
    fn write_manifest(&mut self, file_metadata: Vec<FileMetadata>) -> Result<()> {
        let (f, manifest_file_path) = self.manifest_file()?;
        let mut wbuf = BufWriter::new(f);
        let manifest: Manifest = Manifest::V1(ManifestV1 {
            snapshot_version: 1,
            address_length: ObjectID::LENGTH as u64,
            file_metadata,
            epoch: self.epoch,
        });
        let serialized_manifest = bcs::to_bytes(&manifest)?;
        wbuf.write_all(&serialized_manifest)?;
        wbuf.flush()?;
        wbuf.get_ref().sync_data()?;
        let sha3_digest = compute_sha3_checksum(&manifest_file_path)?;
        wbuf.write_all(&sha3_digest)?;
        wbuf.flush()?;
        wbuf.get_ref().sync_data()?;
        let off = wbuf.get_ref().stream_position()?;
        wbuf.get_ref().set_len(off)?;
        self.local_staging_dir.sync_data()?;
        Ok(())
    }
    fn manifest_file(&mut self) -> Result<(File, PathBuf)> {
        let manifest_file_path = path_to_filesystem(
            self.local_staging_dir_root.clone(),
            &self.epoch_dir().child("MANIFEST"),
        )?;
        let manifest_file_tmp_path = path_to_filesystem(
            self.local_staging_dir_root.clone(),
            &self.epoch_dir().child("MANIFEST.tmp"),
        )?;
        let mut f = File::create(manifest_file_tmp_path.clone())?;
        let mut metab = vec![0u8; MAGIC_BYTES];
        BigEndian::write_u32(&mut metab, MANIFEST_FILE_MAGIC);
        f.rewind()?;
        f.write_all(&metab)?;
        drop(f);
        fs::rename(manifest_file_tmp_path, manifest_file_path.clone())?;
        self.local_staging_dir.sync_data()?;
        let mut f = OpenOptions::new()
            .append(true)
            .open(manifest_file_path.clone())?;
        f.seek(SeekFrom::Start(MAGIC_BYTES as u64))?;
        Ok((f, manifest_file_path))
    }
    fn bucket_func(_object: &LiveObject) -> u32 {
        // TODO: Use the hash bucketing function used for accumulator tree if there is one
        1u32
    }
    fn epoch_dir(&self) -> Path {
        Path::from(format!("epoch_{}", self.epoch))
    }
}
