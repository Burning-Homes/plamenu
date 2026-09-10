CREATE TABLE account_identity_proofs (
    account_id BIGINT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    documents JSONB NOT NULL CHECK (jsonb_typeof(documents) = 'array'
        AND jsonb_array_length(documents) <= 10
        AND octet_length(documents::text) <= 10 * 16384 * 2 + 20)
);
