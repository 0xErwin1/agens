pub fn parse_toml_document(input: &str) -> Result<toml::Table, toml::de::Error> {
    input.parse()
}
