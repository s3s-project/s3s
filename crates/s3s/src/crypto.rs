use numeric_cast::TruncatingCast;

pub trait Checksum {
    type Output: AsRef<[u8]>;

    #[must_use]
    fn new() -> Self;

    fn update(&mut self, data: &[u8]);

    #[must_use]
    fn finalize(self) -> Self::Output;

    #[must_use]
    fn checksum(data: &[u8]) -> Self::Output
    where
        Self: Sized,
    {
        let mut hasher = Self::new();
        hasher.update(data);
        hasher.finalize()
    }
}

pub struct Crc32(crc_fast::Digest);

impl Default for Crc32 {
    fn default() -> Self {
        Self(crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32IsoHdlc))
    }
}

impl Checksum for Crc32 {
    type Output = [u8; 4];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        self.0.finalize().truncating_cast::<u32>().to_be_bytes()
    }
}

pub struct Crc32c(crc_fast::Digest);

impl Default for Crc32c {
    fn default() -> Self {
        Self(crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi))
    }
}

impl Checksum for Crc32c {
    type Output = [u8; 4];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        self.0.finalize().truncating_cast::<u32>().to_be_bytes()
    }
}

pub struct Crc64Nvme(crc_fast::Digest);

impl Default for Crc64Nvme {
    fn default() -> Self {
        Self(crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64Nvme))
    }
}

impl Checksum for Crc64Nvme {
    type Output = [u8; 8];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        self.0.finalize().to_be_bytes()
    }
}

#[derive(Default)]
pub struct Sha1(sha1::Sha1);

impl Checksum for Sha1 {
    type Output = [u8; 20];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        use sha1::Digest as _;
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        use sha1::Digest as _;
        self.0.finalize().into()
    }
}

#[derive(Default)]
pub struct Sha256(sha2::Sha256);

impl Checksum for Sha256 {
    type Output = [u8; 32];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        use sha2::Digest as _;
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        use sha2::Digest as _;
        self.0.finalize().into()
    }
}

#[derive(Default)]
pub struct Md5(md5::Md5);

impl Checksum for Md5 {
    type Output = [u8; 16];

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, data: &[u8]) {
        use md5::Digest as _;
        self.0.update(data);
    }

    fn finalize(self) -> Self::Output {
        use md5::Digest as _;
        self.0.finalize().into()
    }
}
