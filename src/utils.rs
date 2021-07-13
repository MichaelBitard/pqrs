use crate::errors::PQRSError;
use crate::errors::PQRSError::CouldNotOpenFile;
use arrow::{datatypes::Schema, record_batch::RecordBatch};
use log::debug;
use parquet::arrow::{ArrowReader, ArrowWriter, ParquetFileArrowReader};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Row;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::fs::File;
use std::ops::Add;
use std::path::Path;
use std::sync::Arc;

// calculate the sizes in bytes for one KiB, MiB, GiB, TiB, PiB
static ONE_KI_B: i64 = 1024;
static ONE_MI_B: i64 = ONE_KI_B * 1024;
static ONE_GI_B: i64 = ONE_MI_B * 1024;
static ONE_TI_B: i64 = ONE_GI_B * 1024;
static ONE_PI_B: i64 = ONE_TI_B * 1024;

/// Check if a particular path is present on the filesystem
pub fn check_path_present(file_path: &str) -> bool {
    Path::new(file_path).exists()
}

/// Open the file based on the pat and return the File object, else return error
pub fn open_file(file_name: &str) -> Result<File, PQRSError> {
    let path = Path::new(&file_name);
    let file = match File::open(&path) {
        Err(_) => return Err(CouldNotOpenFile(file_name.to_string())),
        Ok(f) => f,
    };

    Ok(file)
}

/// Print the given number of records in either json or json-like format
pub fn print_rows(
    file: File,
    num_records: Option<i64>,
    json: bool,
) -> Result<(), PQRSError> {
    let parquet_reader = SerializedFileReader::new(file)?;
    // get_row_iter allows us to iterate the parquet file one record at a time
    let mut iter = parquet_reader.get_row_iter(None)?;

    let mut start: i64 = 0;
    let end: i64 = num_records.unwrap_or(0);
    // if num_records is None, print all the files
    let all_records = num_records.is_none();

    // print either all records, or the requested number of records
    while all_records || start < end {
        match iter.next() {
            Some(row) => {
                println!("CurrentRow {} {}", start, end);
                print_row(&row, json);
            }
            None => break,
        }
        start += 1;
    }

    Ok(())
}

/// Print the random sample of given size in either json or json-like format
pub fn print_rows_random(
    file: File,
    sample_size: i64,
    json: bool,
) -> Result<(), PQRSError> {
    let parquet_reader = SerializedFileReader::new(file.try_clone()?)?;
    let mut iter = parquet_reader.get_row_iter(None)?;

    // find the number of records present in the file
    let total_records_in_file: i64 = get_row_count(file)?;
    // push all the indexes into the vector initially
    let mut indexes = (0..total_records_in_file).collect::<Vec<_>>();
    debug!("Original indexes: {:?}", indexes);

    // shuffle the indexes to randomize the vector
    let mut rng = thread_rng();
    indexes.shuffle(&mut rng);
    debug!("Shuffled indexes: {:?}", indexes);

    // take only the given number of records from the vector
    indexes = indexes
        .into_iter()
        .take(sample_size as usize)
        .collect::<Vec<_>>();

    debug!("Sampled indexes: {:?}", indexes);

    let mut start: i64 = 0;
    while let Some(row) = iter.next() {
        if indexes.contains(&start) {
            print_row(&row, json)
        }
        start += 1;
    }

    Ok(())
}

/// A representation of Parquet file in a form that can be used for merging
#[derive(Debug)]
pub struct ParquetData {
    /// The schema of the parquet file
    pub schema: Schema,
    /// Collection of the record batches in the parquet file
    pub batches: Vec<RecordBatch>,
    /// The number of rows present in the parquet file
    pub rows: usize,
}

impl Add for ParquetData {
    type Output = Self;

    /// Combine two given parquet files
    fn add(mut self, mut rhs: Self) -> Self::Output {
        // the combined data contains data from both the structs
        let mut combined_data = Vec::new();
        combined_data.append(&mut self.batches);
        combined_data.append(&mut rhs.batches);

        Self {
            // the schema from the lhs is maintained, the assumption is that this
            // method is used only on files that share the same schema
            schema: self.schema,
            batches: combined_data,
            rows: self.rows + rhs.rows,
        }
    }
}

/// Return the row batches, rows and schema for a given parquet file
pub fn get_row_batches(input: &str) -> Result<ParquetData, PQRSError> {
    let file = open_file(input)?;
    let file_reader = SerializedFileReader::new(file).unwrap();
    let mut arrow_reader = ParquetFileArrowReader::new(Arc::new(file_reader));

    let schema = arrow_reader.get_schema()?;
    let record_batch_reader = arrow_reader.get_record_reader(1024)?;
    let mut batches: Vec<RecordBatch> = Vec::new();

    let mut rows = 0;
    for maybe_batch in record_batch_reader {
        let record_batch = maybe_batch.unwrap();
        rows += record_batch.num_rows();

        batches.push(record_batch);
    }

    Ok(ParquetData {
        schema,
        batches,
        rows,
    })
}

/// Write a parquet file to the output location based on the given parquet input
pub fn write_parquet(data: ParquetData, output: &str) -> Result<(), PQRSError> {
    let file = File::create(output)?;
    let fields = data.schema.fields().to_vec();
    // the schema from the record batch might not contain the file specific metadata
    // drop the schema to make sure that we don't fail in that case
    let schema_without_metadata = Schema::new(fields);

    let mut writer = ArrowWriter::try_new(file, Arc::new(schema_without_metadata), None)?;

    // write record batches one at a time
    // record batches are not combined
    for record_batch in data.batches.iter() {
        writer.write(&record_batch)?;
    }

    // closing the writer writes out the FileMetaData
    // if the writer is not closed properly, the metadata footer needed by the parquet
    // format would be corrupt
    writer.close()?;
    Ok(())
}

/// Print the given parquet rows in json or json-like format
fn print_row(row: &Row, use_json: bool) {
    if use_json {
        println!("{}", row.to_json_value());
    } else {
        println!("{}", row.to_string());
    }
}

/// Return the number of rows in the given parquet file
pub fn get_row_count(file: File) -> Result<i64, PQRSError> {
    let parquet_reader = SerializedFileReader::new(file)?;
    let row_group_metadata = parquet_reader.metadata().row_groups();
    // The parquet file is made up of blocks (also called row groups)
    // The row group metadata contains information about all the row groups present in the data
    // Each row group maintains the number of rows present in the block
    // Summing across all the row groups contains the total number of rows present in the file
    let total_num_rows = row_group_metadata.iter().map(|rg| rg.num_rows()).sum();

    Ok(total_num_rows)
}

/// Return the uncompressed and compressed size of the given file
pub fn get_size(file: File) -> Result<(i64, i64), PQRSError> {
    let parquet_reader = SerializedFileReader::new(file)?;
    let row_group_metadata = parquet_reader.metadata().row_groups();

    // Parquet format compresses data at a column level.
    // To calculate the size of the file (compressed or uncompressed), we need to sum
    // across all the row groups present in the parquet file. This is similar to how
    // we calculate the row count in the method above.
    // Do note that this size does not take the footer size into consideration.
    let uncompressed_size = row_group_metadata
        .iter()
        .map(|rg| rg.total_byte_size())
        .sum();
    let compressed_size = row_group_metadata
        .iter()
        .map(|rg| rg.compressed_size())
        .sum();

    Ok((uncompressed_size, compressed_size))
}

/// Pretty print the given size using human readable format
pub fn get_pretty_size(bytes: i64) -> String {
    if bytes / ONE_KI_B < 1 {
        return format!("{} Bytes", bytes);
    }

    if bytes / ONE_MI_B < 1 {
        return format!("{:.3} KiB", bytes / ONE_KI_B);
    }

    if bytes / ONE_GI_B < 1 {
        return format!("{:.3} MiB", bytes / ONE_MI_B);
    }

    if bytes / ONE_TI_B < 1 {
        return format!("{:.3} GiB", bytes / ONE_GI_B);
    }

    if bytes / ONE_PI_B < 1 {
        return format!("{:.3} TiB", bytes / ONE_TI_B);
    }

    return format!("{:.3} PiB", bytes / ONE_PI_B);
}
