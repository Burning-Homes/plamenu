-- A lease-scoped private scratch directory, never a client-visible media key.
ALTER TABLE music_renditions ADD COLUMN progress_dir TEXT
    CHECK(progress_dir ~ '^av-[A-Za-z0-9]{6,64}$');
