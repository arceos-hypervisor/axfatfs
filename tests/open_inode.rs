//! Caller-managed open handles without kernel types or external image fixtures.

use std::cell::{Cell, RefCell};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::rc::Rc;

use axfatfs as fatfs;
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions, StdIoWrapper};
use fscommon::BufStream;

#[derive(Clone)]
struct Disk {
    bytes: Rc<RefCell<Cursor<Vec<u8>>>>,
    fail: Rc<Cell<bool>>,
}

impl Read for Disk {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.bytes.borrow_mut().read(bytes)
    }
}

impl Seek for Disk {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.bytes.borrow_mut().seek(position)
    }
}

impl Write for Disk {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail.get() {
            return Err(io::Error::new(io::ErrorKind::Other, "injected write error"));
        }
        self.bytes.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail.get() {
            Err(io::Error::new(io::ErrorKind::Other, "injected flush error"))
        } else {
            Ok(())
        }
    }
}

type Fs = FileSystem<StdIoWrapper<BufStream<Disk>>>;

fn mount(bytes: Vec<u8>) -> (Fs, Disk) {
    let disk = Disk {
        bytes: Rc::new(RefCell::new(Cursor::new(bytes))),
        fail: Rc::new(Cell::new(false)),
    };
    let fs = FileSystem::new(StdIoWrapper::new(BufStream::new(disk.clone())), FsOptions::new()).unwrap();
    (fs, disk)
}

fn filesystem(kind: FatType) -> (Fs, Disk) {
    let size = match kind {
        FatType::Fat12 => 256 * 1024,
        FatType::Fat16 => 8 * 1024 * 1024,
        FatType::Fat32 => 64 * 1024 * 1024,
    };
    let mut bytes = StdIoWrapper::new(Cursor::new(vec![0; size]));
    fatfs::format_volume(&mut bytes, FormatVolumeOptions::new().fat_type(kind)).unwrap();
    let (fs, disk) = mount(bytes.into_inner().into_inner());
    assert_eq!(fs.fat_type(), kind);
    (fs, disk)
}

fn contents(mut file: impl Read + Seek) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn unlink_keeps_open_data_until_purge_and_does_not_overwrite_a_reused_slot() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let (fs, disk) = filesystem(kind);
        let mut root = fs.root_dir();
        let free_before = fs.stats().unwrap().free_clusters();
        let mut file = root.create_file("old.txt").unwrap();
        let position = file.entry_position().unwrap();
        assert!(matches!(file.purge(), Err(fatfs::Error::InvalidInput)));
        file.write_all(b"original").unwrap();
        root.remove_entry("old.txt").unwrap();
        file.detach();
        assert!(file.is_detached());
        assert_eq!(file.entry_position(), None);
        assert_eq!(fs.stats().unwrap().free_clusters(), free_before - 1);
        let mut replacement = root.create_file("new.txt").unwrap();
        assert_eq!(replacement.entry_position(), Some(position));
        replacement.write_all(b"replacement").unwrap();
        replacement.flush().unwrap();
        file.write_all(b" still open").unwrap();
        file.flush().unwrap();
        assert_eq!(contents(&mut file), b"original still open");
        root.flush().unwrap();
        let (saved, _) = mount(disk.bytes.borrow().get_ref().clone());
        assert!(matches!(
            saved.root_dir().open_file("old.txt"),
            Err(fatfs::Error::NotFound)
        ));
        assert_eq!(contents(saved.root_dir().open_file("new.txt").unwrap()), b"replacement");
        file.purge().unwrap();
        assert_eq!(fs.stats().unwrap().free_clusters(), free_before - 1);
    }
}

#[test]
fn rename_rebinds_cached_metadata_and_keeps_the_open_cursor() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let (fs, disk) = filesystem(kind);
        let mut root = fs.root_dir();
        let mut file = root.create_file("old.txt").unwrap();
        file.write_all(b"before").unwrap();
        file.flush_metadata().unwrap();
        let entry = root.rename_entry("old.txt", &root, "new.txt").unwrap();
        file.attach(&entry).unwrap();
        assert_eq!(file.entry_position(), Some(entry.position()));
        root.create_file("old.txt").unwrap().write_all(b"replacement").unwrap();
        file.write_all(b" after").unwrap();
        file.flush().unwrap();
        root.flush().unwrap();
        let (saved, _) = mount(disk.bytes.borrow().get_ref().clone());
        assert_eq!(
            contents(saved.root_dir().open_file("new.txt").unwrap()),
            b"before after"
        );
        assert_eq!(contents(saved.root_dir().open_file("old.txt").unwrap()), b"replacement");
    }
}

#[test]
fn directory_handles_can_be_rebound_detached_and_reclaimed() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let (fs, _) = filesystem(kind);
        let root = fs.root_dir();
        let free_before = fs.stats().unwrap().free_clusters();
        let mut directory = root.create_dir("old").unwrap();
        assert!(matches!(directory.purge(), Err(fatfs::Error::InvalidInput)));
        directory.flush_metadata().unwrap();
        let entry = root.rename_entry("old", &root, "new").unwrap();
        directory.attach(&entry).unwrap();
        assert_eq!(directory.entry_position(), Some(entry.position()));
        root.remove_entry("new").unwrap();
        directory.detach();
        assert!(directory.is_detached());
        assert_eq!(fs.stats().unwrap().free_clusters(), free_before - 1);
        directory.flush().unwrap();
        directory.purge().unwrap();
        assert_eq!(fs.stats().unwrap().free_clusters(), free_before);
        let mut root = root;
        assert!(matches!(root.purge(), Err(fatfs::Error::InvalidInput)));
    }
}

#[test]
fn directory_flush_drains_buffered_entries_and_propagates_storage_errors() {
    let (fs, disk) = filesystem(FatType::Fat12);
    let mut root = fs.root_dir();
    let pending = root.create_file("entry").unwrap();
    disk.fail.set(true);
    assert!(root.flush().is_err());
    disk.fail.set(false);
    root.flush().unwrap();
    let (saved, _) = mount(disk.bytes.borrow().get_ref().clone());
    assert!(saved.root_dir().open_file("entry").is_ok());
    drop(pending);
}
