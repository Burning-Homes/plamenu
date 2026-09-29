-- Disposable browser catalogue authority fixture; no codec/worker claims.
-- Older visible matches must remain reachable behind 5,500 blocked matches.
-- These small IDs are isolated from live Snowflake allocation and draft IDs.
\set ON_ERROR_STOP on
BEGIN;
INSERT INTO accounts(id,username,display_name,public_key,actor_type)
SELECT 8987,'catalogue_hidden_fixture','Hidden fixture author',public_key,'Person'
FROM accounts WHERE username='developer' AND domain IS NULL;
INSERT INTO accounts(id,username,display_name,public_key,actor_type)
SELECT 8988,'catalogue_hidden_films','Hidden fixture channel',public_key,'Group'
FROM accounts WHERE id=8987;
INSERT INTO video_channels(account_id,owner_account_id) VALUES(8988,8987);
INSERT INTO blocks(id,account_id,target_account_id)
SELECT 8989,id,8987 FROM accounts WHERE username='developer' AND domain IS NULL;
CREATE TEMP TABLE catalogue_fixture AS
SELECT 1000000+n AS sid,2000000+n AS mid,
    CASE WHEN n<=50 THEN a.id ELSE 8987 END AS owner,
    CASE WHEN n<=50 THEN 8999 ELSE 8988 END AS channel,
    CASE WHEN n<=50 THEN 'Catalogue boundary visible fixture' ELSE 'Catalogue boundary hidden fixture' END AS title
FROM generate_series(1,5550) n CROSS JOIN accounts a
WHERE a.username='developer' AND a.domain IS NULL;
INSERT INTO statuses(id,account_id,object_type,title,content)
SELECT sid,owner,'Video',title,'Authorization fixture only.' FROM catalogue_fixture;
INSERT INTO media_attachments(id,account_id,status_id,file_name,content_type,kind)
SELECT mid,owner,sid,'catalogue-fixture.mp4','video/mp4','video' FROM catalogue_fixture;
INSERT INTO video_publications(status_id,channel_id,media_id)
SELECT sid,channel,mid FROM catalogue_fixture;
COMMIT;
