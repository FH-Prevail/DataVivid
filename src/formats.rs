//! The file formats DataVivid understands, independent of how they are read.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Format {
    Csv,
    Tsv,
    Parquet,
    Json,
    NdJson,
    ArrowIpc,
    Avro,
    Excel,
    Ods,
    Sqlite,
}

impl Format {
    pub const ALL: [Format; 10] = [
        Format::Csv,
        Format::Tsv,
        Format::Parquet,
        Format::Json,
        Format::NdJson,
        Format::ArrowIpc,
        Format::Avro,
        Format::Excel,
        Format::Ods,
        Format::Sqlite,
    ];

    /// Human-readable name, used in dialogs and as the Explorer type name.
    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV (comma-separated values)",
            Format::Tsv => "TSV (tab-separated values)",
            Format::Parquet => "Apache Parquet",
            Format::Json => "JSON (array of records)",
            Format::NdJson => "JSON Lines / NDJSON",
            Format::ArrowIpc => "Apache Arrow IPC / Feather",
            Format::Avro => "Apache Avro",
            Format::Excel => "Excel workbook",
            Format::Ods => "OpenDocument spreadsheet",
            Format::Sqlite => "SQLite database",
        }
    }

    /// Short name used for ProgIDs, e.g. `DataVivid.Parquet`.
    pub fn key(self) -> &'static str {
        match self {
            Format::Csv => "Csv",
            Format::Tsv => "Tsv",
            Format::Parquet => "Parquet",
            Format::Json => "Json",
            Format::NdJson => "NdJson",
            Format::ArrowIpc => "Arrow",
            Format::Avro => "Avro",
            Format::Excel => "Excel",
            Format::Ods => "Ods",
            Format::Sqlite => "Sqlite",
        }
    }

    /// Extensions (lower case, without dot). The first one is the default
    /// when saving.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Csv => &["csv"],
            Format::Tsv => &["tsv", "tab"],
            Format::Parquet => &["parquet", "pq"],
            Format::Json => &["json"],
            Format::NdJson => &["jsonl", "ndjson"],
            Format::ArrowIpc => &["arrow", "feather", "ipc"],
            Format::Avro => &["avro"],
            Format::Excel => &["xlsx", "xlsm", "xlsb", "xls"],
            Format::Ods => &["ods"],
            Format::Sqlite => &["sqlite", "sqlite3", "db"],
        }
    }

    /// Whether DataVivid can write this format.
    pub fn can_write(self) -> bool {
        !matches!(self, Format::Ods)
    }

    /// The format for a path, by extension. Compressed text files such as
    /// `data.csv.gz` are recognised by their inner extension.
    pub fn from_path(path: &Path) -> Option<Format> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        let name = name
            .strip_suffix(".gz")
            .or_else(|| name.strip_suffix(".zst"))
            .unwrap_or(&name);
        let ext = name.rsplit_once('.')?.1;
        Self::from_extension(ext)
    }

    pub fn from_extension(ext: &str) -> Option<Format> {
        let ext = ext.trim_start_matches('.').to_lowercase();
        if matches!(ext.as_str(), "txt" | "psv" | "dat") {
            return Some(Format::Csv); // delimited text; separator is sniffed
        }
        Self::ALL
            .into_iter()
            .find(|f| f.extensions().contains(&ext.as_str()))
    }

    /// Whether the format is delimited text (gets the import-options dialog).
    pub fn is_delimited(self) -> bool {
        matches!(self, Format::Csv | Format::Tsv)
    }
}

/// Extensions offered in the "Open" dialog, including extras such as `.txt`
/// that are opened but not registered with Windows.
pub fn open_dialog_extensions() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Format::ALL
        .iter()
        .flat_map(|f| f.extensions().iter().copied())
        .collect();
    v.extend(["txt", "psv", "gz"]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats() {
        assert_eq!(Format::from_path(Path::new("a/b.CSV")), Some(Format::Csv));
        assert_eq!(Format::from_path(Path::new("x.csv.gz")), Some(Format::Csv));
        assert_eq!(Format::from_path(Path::new("x.parquet")), Some(Format::Parquet));
        assert_eq!(Format::from_path(Path::new("x.feather")), Some(Format::ArrowIpc));
        assert_eq!(Format::from_path(Path::new("x.jsonl")), Some(Format::NdJson));
        assert_eq!(Format::from_path(Path::new("x.xlsx")), Some(Format::Excel));
        assert_eq!(Format::from_path(Path::new("noext")), None);
    }
}
