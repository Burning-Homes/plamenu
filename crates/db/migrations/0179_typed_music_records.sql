-- Fixed-shape music snapshots are typed PostgreSQL records. Their JSON API
-- representation remains a serialization boundary, not the storage format.
-- Live state, ownership, leases, grants and revisions stay in ordinary columns.
CREATE TYPE music_remote_details AS (
    artist TEXT, artist_uri TEXT, album TEXT, genre TEXT,
    release_year INTEGER, track_number INTEGER, disc_number INTEGER,
    musicbrainz_id TEXT, tags TEXT[], duration DOUBLE PRECISION,
    image_url TEXT, audio_type TEXT
);
ALTER TABLE music_remote_resources ALTER COLUMN details DROP DEFAULT;
ALTER TABLE music_remote_resources DROP CONSTRAINT IF EXISTS music_remote_resources_details_check;
ALTER TABLE music_remote_resources ALTER COLUMN details TYPE music_remote_details
 USING jsonb_populate_record(ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,details);
ALTER TABLE music_remote_resources ALTER COLUMN details SET DEFAULT
 ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details;
ALTER TABLE music_remote_track_proofs DROP CONSTRAINT IF EXISTS music_remote_track_proofs_details_check;
ALTER TABLE music_remote_track_proofs ALTER COLUMN details TYPE music_remote_details
 USING jsonb_populate_record(ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,details);

ALTER TABLE music_remote_resources ADD CONSTRAINT music_remote_details_budget
 CHECK(octet_length(to_jsonb(details)::text)<=32768);
ALTER TABLE music_remote_track_proofs ADD CONSTRAINT music_remote_proof_details_budget
 CHECK(octet_length(to_jsonb(details)::text)<=32768);

CREATE TYPE music_scan_options AS (recursive BOOLEAN, extensions TEXT[], exclude TEXT[]);
ALTER TABLE music_local_folders ALTER COLUMN options DROP DEFAULT;
ALTER TABLE music_local_folders DROP CONSTRAINT IF EXISTS music_local_folders_options_check;
ALTER TABLE music_local_folders ALTER COLUMN options TYPE music_scan_options
 USING jsonb_populate_record(ROW(true,ARRAY['mp3','flac','ogg','opus','m4a','aac','wav'],'{}'::TEXT[])::music_scan_options,options);
ALTER TABLE music_local_folders ALTER COLUMN options SET DEFAULT
 ROW(true,ARRAY['mp3','flac','ogg','opus','m4a','aac','wav'],'{}'::TEXT[])::music_scan_options;
ALTER TABLE music_local_folders ADD CONSTRAINT music_scan_options_shape CHECK(
 (options).recursive IS NOT NULL AND (options).extensions IS NOT NULL AND (options).exclude IS NOT NULL
 AND cardinality((options).extensions)<=11 AND cardinality((options).exclude)<=32);

-- Source selection is an immutable, ordered recipe, including repeats. These
-- typed identities deliberately have no source FK: withdrawal must not erase
-- the recipe before its current authority checks can report an unavailable item.
CREATE TYPE music_download_entry AS (
 remote_id BIGINT,id BIGINT,collection_id BIGINT,asset_id BIGINT,
 status_id BIGINT,episode_id BIGINT,position INTEGER
);
CREATE TYPE music_download_parent AS (kind TEXT,id BIGINT,revision BIGINT);
CREATE TYPE music_download_item AS (entry music_download_entry,proof TEXT);
CREATE TYPE music_download_selection AS (items music_download_item[],parent music_download_parent);
CREATE TYPE music_download_item_error AS (index INTEGER,message TEXT);
ALTER TABLE music_downloads DROP CONSTRAINT IF EXISTS music_downloads_selection_check;
ALTER TABLE music_downloads ALTER COLUMN selection TYPE music_download_selection
 USING jsonb_populate_record(NULL::music_download_selection,selection);
ALTER TABLE music_downloads ADD CONSTRAINT music_download_selection_shape CHECK(
 (selection).items IS NOT NULL AND cardinality((selection).items) BETWEEN 1 AND 500);
ALTER TABLE music_downloads ALTER COLUMN item_errors DROP DEFAULT;
ALTER TABLE music_downloads DROP CONSTRAINT IF EXISTS music_downloads_item_errors_check;
CREATE FUNCTION music_download_errors_from_json(input JSONB) RETURNS music_download_item_error[]
 LANGUAGE SQL IMMUTABLE STRICT AS $$
 SELECT COALESCE(array_agg(ROW((entry->>'index')::INTEGER,entry->>'message')::music_download_item_error ORDER BY position),'{}'::music_download_item_error[])
 FROM jsonb_array_elements(input) WITH ORDINALITY AS items(entry,position)
$$;
ALTER TABLE music_downloads ALTER COLUMN item_errors TYPE music_download_item_error[]
 USING music_download_errors_from_json(item_errors);
DROP FUNCTION music_download_errors_from_json(JSONB);
ALTER TABLE music_downloads ALTER COLUMN item_errors SET DEFAULT '{}'::music_download_item_error[];
ALTER TABLE music_downloads ADD CONSTRAINT music_download_errors_shape CHECK(cardinality(item_errors)<=500);

-- POSIX identities and unsigned byte counts use lossless decimal text inside
-- typed records, matching the full u64 Rust range without signed truncation.
CREATE TYPE music_mount_identity AS (device TEXT,inode TEXT);
CREATE TYPE music_file_fingerprint AS (
 device TEXT,inode TEXT,bytes TEXT,modified_seconds BIGINT,modified_nanos BIGINT
);
CREATE TYPE music_local_mutation_intent AS (
 root_id TEXT,mount music_mount_identity,parent music_mount_identity,
 source TEXT,target TEXT,before music_file_fingerprint,
 after music_file_fingerprint,operation TEXT
);
ALTER TABLE music_local_mutations DROP CONSTRAINT IF EXISTS music_local_mutations_intent_check;
ALTER TABLE music_local_mutations ALTER COLUMN intent TYPE music_local_mutation_intent USING
 ROW(intent->>'root_id',
 ROW(intent->'mount'->>0,intent->'mount'->>1)::music_mount_identity,
 ROW(intent->'parent'->>0,intent->'parent'->>1)::music_mount_identity,
 intent->>'source',intent->>'target',
 jsonb_populate_record(NULL::music_file_fingerprint,intent->'before'),
 CASE WHEN jsonb_typeof(intent->'after')='object' THEN jsonb_populate_record(NULL::music_file_fingerprint,intent->'after') ELSE NULL END,
 intent->>'operation')::music_local_mutation_intent;
ALTER TABLE music_local_mutations ADD CONSTRAINT music_local_mutation_shape CHECK(
 (intent).root_id IS NOT NULL AND (intent).source IS NOT NULL AND (intent).target IS NOT NULL
 AND (intent).operation IN ('rename','delete','tags')
 AND ((intent).operation<>'tags' OR (intent).after IS DISTINCT FROM NULL));

CREATE OR REPLACE FUNCTION retire_account_remote_music() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_saved_remote WHERE account_id=NEW.id;
        DELETE FROM music_remote_track_proofs WHERE controller_id=NEW.id;
        UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),tombstoned_at=clock_timestamp(),changed_at=clock_timestamp(),
            name='',summary='',details=ROW('',NULL,'','',NULL,NULL,NULL,NULL,'{}'::TEXT[],NULL,NULL,NULL)::music_remote_details,audio_url=NULL,next_page_uri=NULL WHERE controller_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
