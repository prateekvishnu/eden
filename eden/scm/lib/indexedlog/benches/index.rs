/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::Arc;

use indexedlog::index::InsertKey;
use indexedlog::index::InsertValue;
use indexedlog::index::OpenOptions;
use minibench::bench;
use minibench::elapsed;
use minibench::measure;
use minibench::Measure;
use rand_chacha::rand_core::RngCore;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaChaRng;
use tempfile::tempdir;

const N: usize = 204800;

/// Generate random buffer
fn gen_buf(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    ChaChaRng::seed_from_u64(0).fill_bytes(buf.as_mut());
    buf
}

/// Default open options: 4K checksum chunk
fn open_opts() -> OpenOptions {
    let mut open_opts = OpenOptions::new();
    open_opts.checksum_chunk_size_logarithm(12);
    open_opts
}

fn main() {
    bench("index insertion (owned key)", || {
        let dir = tempdir().unwrap();
        let mut idx = open_opts().open(dir.path().join("i")).expect("open");
        let buf = gen_buf(N * 20);
        elapsed(move || {
            for i in 0..N {
                idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                    .expect("insert");
            }
        })
    });

    bench("index insertion (referred key)", || {
        let dir = tempdir().unwrap();
        let buf = gen_buf(N * 20);
        let mut idx = open_opts()
            .key_buf(Some(Arc::new(buf.clone())))
            .open(dir.path().join("i"))
            .expect("open");
        elapsed(move || {
            for i in 0..N {
                idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                    .expect("insert");
            }
        })
    });

    bench("index flush", || {
        let dir = tempdir().unwrap();
        let mut idx = open_opts().open(dir.path().join("i")).expect("open");
        let buf = gen_buf(N * 20);
        for i in 0..N {
            idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                .expect("insert");
        }
        elapsed(|| {
            idx.flush().expect("flush");
        })
    });

    {
        let dir = tempdir().unwrap();
        let mut idx = open_opts().open(dir.path().join("i")).expect("open");
        let buf = gen_buf(N * 20);
        for i in 0..N {
            idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                .expect("insert");
        }

        bench("index lookup (memory)", || {
            elapsed(|| {
                for i in 0..N {
                    idx.get(&&buf[20 * i..20 * (i + 1)]).expect("lookup");
                }
            })
        });

        bench("index prefix scan (2B)", || {
            elapsed(|| {
                for _ in 0..(N / 3) {
                    idx.scan_prefix([0x33, 0x33]).unwrap().count();
                }
            })
        });

        bench("index prefix scan (1B)", || {
            elapsed(|| {
                for _ in 0..(N / 807) {
                    idx.scan_prefix([0x33]).unwrap().count();
                }
            })
        });
    }

    {
        let dir = tempdir().unwrap();
        let mut idx = open_opts()
            .checksum_enabled(false)
            .open(dir.path().join("i"))
            .expect("open");
        let buf = gen_buf(N * 20);
        for i in 0..N {
            idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                .expect("insert");
        }
        idx.flush().expect("flush");

        bench("index lookup (disk, no verify)", || {
            elapsed(|| {
                for i in 0..N {
                    idx.get(&&buf[20 * i..20 * (i + 1)]).expect("lookup");
                }
            })
        });

        bench("index prefix scan (2B, disk)", || {
            elapsed(|| {
                for _ in 0..(N / 3) {
                    idx.scan_prefix([0x33, 0x33]).unwrap().count();
                }
            })
        });

        bench("index prefix scan (1B, disk)", || {
            elapsed(|| {
                for _ in 0..(N / 807) {
                    idx.scan_prefix([0x33]).unwrap().count();
                }
            })
        });
    }

    bench("index lookup (disk, verified)", || {
        let dir = tempdir().unwrap();
        let mut idx = open_opts().open(dir.path().join("i")).expect("open");
        let buf = gen_buf(N * 20);
        for i in 0..N {
            idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                .expect("insert");
        }
        idx.flush().expect("flush");
        elapsed(move || {
            for i in 0..N {
                idx.get(&&buf[20 * i..20 * (i + 1)]).expect("lookup");
            }
        })
    });

    bench("index size (5M owned keys)", || {
        const N: usize = 5000000;
        let dir = tempdir().unwrap();
        let mut idx = open_opts().open(dir.path().join("i")).expect("open");
        let buf = gen_buf(N * 20);
        for i in 0..N {
            idx.insert(&&buf[20 * i..20 * (i + 1)], i as u64)
                .expect("insert");
        }
        measure::Bytes::measure(|| idx.flush().unwrap())
    });

    bench("index size (5M referred keys)", || {
        const N: usize = 5000000;
        let dir = tempdir().unwrap();
        let buf = gen_buf(N * 20);
        let mut idx = open_opts()
            .key_buf(Some(Arc::new(buf.clone())))
            .open(dir.path().join("i"))
            .expect("open");
        for i in 0..N {
            let ext_key = InsertKey::Reference((i as u64 * 20, 20));
            idx.insert_advanced(ext_key, InsertValue::Prepend(i as u64))
                .expect("insert");
        }
        measure::Bytes::measure(|| idx.flush().unwrap())
    });
}
