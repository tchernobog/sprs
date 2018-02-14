//! Serialization and deserialization of sparse matrices

use std::path::Path;
use std::io;
use std::io::{BufRead, Write, Seek, SeekFrom};
use std::fs::File;
use std::error::Error;
use std::fmt;

use num_traits::cast::NumCast;

use sparse::{TriMatI, SparseMat};
use indexing::SpIndex;
use num_kinds::{PrimitiveKind, NumKind};

#[derive(Debug)]
pub enum IoError {
    Io(io::Error),
    BadMatrixMarketFile,
    UnsupportedMatrixMarketFormat,
}

use self::IoError::*;

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            IoError::Io(ref err) => err.fmt(f),
            IoError::BadMatrixMarketFile =>
                write!(f, "Bad matrix market file."),
            IoError::UnsupportedMatrixMarketFormat =>
                write!(f, "Bad matrix market file."),
        }
    }
}

impl Error for IoError {
    fn description(&self) -> &str {
        match *self {
            IoError::Io(ref err) => err.description(),
            IoError::BadMatrixMarketFile => "bad matrix market file",
            IoError::UnsupportedMatrixMarketFormat => "unsupported format",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            IoError::Io(ref err) => Some(err),
            IoError::BadMatrixMarketFile => None,
            IoError::UnsupportedMatrixMarketFormat => None,
        }
    }
}

impl From<io::Error> for IoError {
    fn from(err: io::Error) -> IoError {
        IoError::Io(err)
    }
}

impl PartialEq for IoError {
    fn eq(&self, rhs: &IoError) -> bool {
        match *self {
            IoError::BadMatrixMarketFile => match *rhs {
                IoError::BadMatrixMarketFile => true,
                _ => false,
            },
            IoError::UnsupportedMatrixMarketFormat => match *rhs {
                IoError::UnsupportedMatrixMarketFormat => true,
                _ => false,
            },
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq)]
enum DataType {
    Integer,
    Real,
    Complex,
}

#[derive(Debug, PartialEq)]
enum SymmetryMode {
    General,
    Hermitian,
    Symmetric,
    SkewSymmetric,
}

fn parse_header(header: &str) -> Result<(SymmetryMode, DataType), IoError> {
    if !header.starts_with("%%matrixmarket matrix coordinate") {
        return Err(BadMatrixMarketFile);
    }
    let data_type = if header.contains("real") {
        DataType::Real
    } else if header.contains("integer") {
        DataType::Integer
    } else if header.contains("complex") {
        DataType::Complex
    } else {
        return Err(BadMatrixMarketFile);
    };
    let sym_mode = if header.contains("general") {
        SymmetryMode::General
    } else if header.contains("symmetric") {
        SymmetryMode::Symmetric
    } else if header.contains("skew-symmetric") {
        SymmetryMode::SkewSymmetric
    } else if header.contains("hermitian") {
        SymmetryMode::Hermitian
    } else {
        return Err(BadMatrixMarketFile);
    };
    Ok((sym_mode, data_type))
}

/// Read a sparse matrix file in the Matrix Market format and return a
/// corresponding triplet matrix.
///
/// Presently, only general matrices are supported, but symmetric and hermitian
/// matrices should be supported in the future.
pub fn read_matrix_market<N, I, P>(mm_file: P) -> Result<TriMatI<N, I>, IoError>
where I: SpIndex,
      N: NumCast + Clone,
      P: AsRef<Path>,
{
    let mm_file = mm_file.as_ref();
    let f = File::open(mm_file)?;
    let mut reader = io::BufReader::new(f);
    // MatrixMarket format specifies lines of at most 1024 chars
    let mut line = String::with_capacity(1024);

    // Parse the header line, all tags are case insensitive.
    reader.read_line(&mut line)?;
    let header = line.to_lowercase();
    let (sym_mode, data_type) = parse_header(&header)?;
    if data_type == DataType::Complex {
        // we currently don't support complex
        return Err(UnsupportedMatrixMarketFormat);
    }
    if sym_mode == SymmetryMode::Hermitian {
        // support for Hermitian requires complex support
        return Err(UnsupportedMatrixMarketFormat);
    }
    // The header is followed by any number of comment or empty lines, skip
    loop {
        line.clear();
        let len = reader.read_line(&mut line)?;
        if len == 0 || line.starts_with("%") {
            continue;
        } else {
            break;
        }
    }
    // read shape and number of entries
    // this is a line like:
    // rows cols entries
    // with arbitrary amounts of whitespace
    let (rows, cols, entries) = {
        let mut infos = line.split_whitespace()
                            .filter_map(|s| s.parse::<usize>().ok());
        let rows = infos.next().ok_or(BadMatrixMarketFile)?;
        let cols = infos.next().ok_or(BadMatrixMarketFile)?;
        let entries = infos.next().ok_or(BadMatrixMarketFile)?;
        if infos.next().is_some() {
            return Err(BadMatrixMarketFile);
        }
        (rows, cols, entries)
    };
    let nnz_max = if sym_mode == SymmetryMode::General {
        entries
    } else {
        2 * entries
    };
    let mut row_inds = Vec::with_capacity(nnz_max);
    let mut col_inds = Vec::with_capacity(nnz_max);
    let mut data = Vec::with_capacity(nnz_max);
    // one non-zero entry per non-empty line
    for _ in 0..entries {
        // skip empty lines (no comment line should appear)
        loop {
            line.clear();
            let len = reader.read_line(&mut line)?;
            // check for an all whitespace line
            if len != 0 && line.split_whitespace().next() == None {
                continue;
            } else {
                break;
            }
        }
        // Non-zero entries are lines of the form:
        // row col value
        // if the data type is integer of real, and
        // row col real imag
        // if the data type is complex.
        // Again, this is with arbitrary amounts of whitespace
        let mut entry = line.split_whitespace();
        let row = entry.next()
                       .ok_or(BadMatrixMarketFile)
                       .and_then(|s| s.parse::<usize>()
                                      .or(Err(BadMatrixMarketFile)))?;
        let col = entry.next()
                       .ok_or(BadMatrixMarketFile)
                       .and_then(|s| s.parse::<usize>()
                                      .or(Err(BadMatrixMarketFile)))?;
        // MatrixMarket indices are 1-based
        let row = row.checked_sub(1).ok_or(BadMatrixMarketFile)?;
        let col = col.checked_sub(1).ok_or(BadMatrixMarketFile)?;
        let val : N = match data_type {
            DataType::Integer => {
                let val = entry.next()
                               .ok_or(BadMatrixMarketFile)
                               .and_then(|s| s.parse::<isize>()
                                              .or(Err(BadMatrixMarketFile)))?;
                NumCast::from(val).unwrap()
            },
            DataType::Real => {
                let val = entry.next()
                               .ok_or(BadMatrixMarketFile)
                               .and_then(|s| s.parse::<f64>()
                                              .or(Err(BadMatrixMarketFile)))?;
                NumCast::from(val).unwrap()
            },
            DataType::Complex => unreachable!(),
        };
        row_inds.push(I::from_usize(row));
        col_inds.push(I::from_usize(col));
        data.push(val.clone());
        if sym_mode != SymmetryMode::General && row != col {
            if sym_mode == SymmetryMode::Hermitian {
                unreachable!();
            } else {
                row_inds.push(I::from_usize(col));
                col_inds.push(I::from_usize(row));
                data.push(val);
            }
        }
        if sym_mode == SymmetryMode::SkewSymmetric && row == col {
            return Err(BadMatrixMarketFile);
        }
        if entry.next().is_some() {
            return Err(BadMatrixMarketFile);
        }
    }

    Ok(TriMatI::from_triplets((rows, cols), row_inds, col_inds, data))
}

/// Write a sparse matrix into the matrix market format.
///
/// # Example
///
/// ```rust,no_run
/// use sprs::{CsMat};
/// # use std::io;
/// # fn save_id5() -> Result<(), io::Error> {
/// let save_path = "/tmp/identity5.mm";
/// let eye : CsMat<f64> = CsMat::eye(5);
/// sprs::io::write_matrix_market(&save_path, &eye)?;
/// # Ok(())
/// # }
/// ```
pub fn write_matrix_market<'a, N, I, M, P>(path: P, mat: M)
    -> Result<(), io::Error>
where I: 'a + SpIndex + fmt::Display,
      N: 'a + PrimitiveKind + Copy + fmt::Display,
      M: IntoIterator<Item=(&'a N, (I, I))> + SparseMat,
      P: AsRef<Path>,
{
    let (rows, cols, nnz) = (mat.rows(), mat.cols(), mat.nnz());
    let f = File::create(path)?;
    let mut writer = io::BufWriter::new(f);

    // header
    let data_type = match N::num_kind() {
        NumKind::Integer => "integer",
        NumKind::Float => "real",
        NumKind::Complex => "complex",
    };
    write!(writer,
           "%%MatrixMarket matrix coordinate {} general\n",
           data_type)?;
    write!(writer, "% written by sprs\n")?;

    // dimensions and nnz
    write!(writer, "{} {} {}\n", rows, cols, nnz)?;

    // entries
    for (val, (row, col)) in mat.into_iter() {
        write!(writer, "{} {} {}\n", row.index() + 1, col.index() + 1, val)?;
    }
    Ok(())
}

/// Write a symmetric sparse matrix into the matrix market format.
///
/// This function does not enforce the actual symmetry of the matrix,
/// instead only the elements below the diagonal are written.
pub fn write_matrix_market_sym<'a, N, I, M, P>(path: P, mat: M)
    -> Result<(), io::Error>
where I: 'a + SpIndex + fmt::Display,
      N: 'a + PrimitiveKind + Copy + fmt::Display,
      M: IntoIterator<Item=(&'a N, (I, I))> + SparseMat,
      P: AsRef<Path>,
{
    let (rows, cols, nnz) = (mat.rows(), mat.cols(), mat.nnz());
    let f = File::create(path)?;
    let mut writer = io::BufWriter::new(f);

    // header
    let data_type = match N::num_kind() {
        NumKind::Integer => "integer",
        NumKind::Float => "real",
        NumKind::Complex => "complex",
    };
    write!(writer,
           "%%MatrixMarket matrix coordinate {} symmetric\n",
           data_type)?;
    write!(writer, "% written by sprs\n")?;

    // record current position to be able to rewrite nnz
    let dim_header_pos = writer.seek(SeekFrom::Current(0))?;
    // dimensions and nnz
    write!(writer, "{} {} {}\n", rows, cols, nnz)?;

    // entries
    let mut entries = 0;
    for (val, (row, col)) in mat.into_iter().filter(|&(_, (r, c))| r <= c) {
        write!(writer, "{} {} {}\n", row.index() + 1, col.index() + 1, val)?;
        entries += 1;
    }
    assert!(entries <= nnz);
    writer.seek(SeekFrom::Start(dim_header_pos))?;
    write!(writer, "{} {} {}", rows, cols, entries)?;
    let dim_header_size = format!("{} {} {}", rows, cols, nnz).len();
    let new_size = format!("{} {} {}", rows, cols, entries).len();
    if new_size < dim_header_size {
        let nb_spaces = dim_header_size - new_size;
        for _ in 0..nb_spaces {
            writer.write(b" ")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::{
        read_matrix_market,
        write_matrix_market,
        write_matrix_market_sym,
        IoError,
    };
    use CsMat;
    use tempdir::TempDir;
    #[test]
    fn simple_matrix_market_read() {
        let path = "data/matrix_market/simple.mm";
        let mat = read_matrix_market::<f64, usize, _>(path).unwrap();
        assert_eq!(mat.rows(), 5);
        assert_eq!(mat.cols(), 5);
        assert_eq!(mat.nnz(), 8);
        assert_eq!(mat.row_inds(), &[0, 1, 2, 0, 3, 3, 3, 4]);
        assert_eq!(mat.col_inds(), &[0, 1, 2, 3, 1, 3, 4, 4]);
        assert_eq!(mat.data(),
                   &[1., 10.5, 1.5e-02, 6., 2.505e2, -2.8e2, 3.332e1, 1.2e+1]);
    }

    #[test]
    fn int_matrix_market_read() {
        let path = "data/matrix_market/simple_int.mm";
        let mat = read_matrix_market::<i32, usize, _>(path).unwrap();
        assert_eq!(mat.rows(), 5);
        assert_eq!(mat.cols(), 5);
        assert_eq!(mat.nnz(), 8);
        assert_eq!(mat.row_inds(), &[0, 1, 2, 0, 3, 3, 3, 4]);
        assert_eq!(mat.col_inds(), &[0, 1, 2, 3, 1, 3, 4, 4]);
        assert_eq!(mat.data(), &[1, 1, 1, 6, 2, -2, 3, 1]);
        // read int, convert to float
        let mat = read_matrix_market::<f32, i16, _>(path).unwrap();
        assert_eq!(mat.rows(), 5);
        assert_eq!(mat.cols(), 5);
        assert_eq!(mat.nnz(), 8);
        assert_eq!(mat.row_inds(), &[0, 1, 2, 0, 3, 3, 3, 4]);
        assert_eq!(mat.col_inds(), &[0, 1, 2, 3, 1, 3, 4, 4]);
        assert_eq!(mat.data(), &[1., 1., 1., 6., 2., -2., 3., 1.]);
    }

    #[test]
    fn matrix_market_read_fail_too_many_in_entry() {
        let path = "data/matrix_market/bad_files/too_many_elems_in_entry.mm";
        let res = read_matrix_market::<f64, i32, _>(path);
        assert_eq!(res.unwrap_err(), IoError::BadMatrixMarketFile);
    }

    #[test]
    fn matrix_market_read_fail_not_enough_entries() {
        let path = "data/matrix_market/bad_files/not_enough_entries.mm";
        let res = read_matrix_market::<f64, i32, _>(path);
        assert_eq!(res.unwrap_err(), IoError::BadMatrixMarketFile);
    }

    #[test]
    fn read_write_read_matrix_market() {
        let path = "data/matrix_market/simple.mm";
        let mat = read_matrix_market::<f64, usize, _>(path).unwrap();
        let tmp_dir = TempDir::new("sprs-tmp").unwrap();
        let save_path = tmp_dir.path().join("simple.mm");
        write_matrix_market(&save_path, mat.view()).unwrap();
        let mat2 = read_matrix_market::<f64, usize, _>(&save_path).unwrap();
        assert_eq!(mat, mat2);
        write_matrix_market(&save_path, &mat2).unwrap();
        let mat3 = read_matrix_market::<f64, usize, _>(&save_path).unwrap();
        assert_eq!(mat, mat3);
    }

    #[test]
    fn read_write_read_matrix_market_via_csc() {
        let path = "data/matrix_market/simple.mm";
        let mat = read_matrix_market::<f64, usize, _>(path).unwrap();
        let csc = mat.to_csc();
        let tmp_dir = TempDir::new("sprs-tmp").unwrap();
        let save_path = tmp_dir.path().join("simple_csc.mm");
        write_matrix_market(&save_path, &csc).unwrap();
        let mat2 = read_matrix_market::<f64, usize, _>(&save_path).unwrap();
        assert_eq!(csc, mat2.to_csc());
    }

    #[test]
    fn read_symmetric_matrix_market() {
        let path = "data/matrix_market/symmetric.mm";
        let mat = read_matrix_market::<f64, usize, _>(path).unwrap();
        let csc = mat.to_csc();
        let expected = CsMat::new_csc((5, 5),
                                      vec![0, 1, 3, 4, 6, 8],
                                      vec![0, 1, 3, 2, 1, 4, 3, 4],
                                      vec![1., 10.5, 2.505e2, 1.5e-2, 2.505e2,
                                           3.332e1, 3.332e1, 1.2e1]);
        assert_eq!(csc, expected);
        let tmp_dir = TempDir::new("sprs-tmp").unwrap();
        let save_path = tmp_dir.path().join("symmetric.mm");
        write_matrix_market_sym(&save_path, &csc).unwrap();
        let mat2 = read_matrix_market::<f64, usize, _>(&save_path).unwrap();
        assert_eq!(csc, mat2.to_csc());
    }

    #[test]
    /// Test whether the seek and replace strategy in the symmetric write
    /// works.
    fn tricky_symmetric_matrix_market() {
        // design a 5x5 symmetric matrix such that the number
        // of nonzeros has more digits than the number of symmetric entries
        // We take the matrix
        // | .  2  .  .  1 |
        // | 2  .  3  .  . |
        // | .  3  .  5  . |
        // | .  .  5  .  4 |
        // | 1  .  .  4  . |
        let mat = CsMat::new((5, 5),
                              vec![0, 2, 4, 6, 8, 10],
                              vec![1, 4, 0, 2, 1, 3, 2, 4, 0, 3],
                              vec![2, 1, 2, 3, 3, 5, 5, 4, 1, 4]);
        let tmp_dir = TempDir::new("sprs-tmp").unwrap();
        let save_path = tmp_dir.path().join("symmetric.mm");
        write_matrix_market_sym(&save_path, &mat).unwrap();
        let mat2 = read_matrix_market::<i32, usize, _>(&save_path).unwrap();
        assert_eq!(mat, mat2.to_csr());
    }
}
