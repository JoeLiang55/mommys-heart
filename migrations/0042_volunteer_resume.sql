-- A volunteer's resume, filed in the document library.
--
-- The bytes live in SharePoint, under the volunteer root
-- (`SHAREPOINT_VOLUNTEER_ROOT_FOLDER`), never here. These columns are only the
-- pointer the app needs to fetch or replace the file, plus the two facts worth
-- showing on the profile without a round trip to the library.
ALTER TABLE volunteers
    ADD COLUMN resume_file_name    TEXT   NOT NULL DEFAULT '',
    ADD COLUMN resume_item_id      TEXT   NOT NULL DEFAULT '',
    ADD COLUMN resume_size_bytes   BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN resume_uploaded_at  TIMESTAMPTZ;
