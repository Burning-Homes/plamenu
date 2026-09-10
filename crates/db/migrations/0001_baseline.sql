-- Plamenu public-release baseline.
--
-- This intentionally replaces the pre-release migration history. Existing pre-release
-- databases must be recreated before running this migration.

--
-- PostgreSQL database dump
--


-- Dumped from database version 18.4
-- Dumped by pg_dump version 18.4

SET LOCAL statement_timeout = 0;
SET LOCAL lock_timeout = 0;
SET LOCAL idle_in_transaction_session_timeout = 0;
SET LOCAL transaction_timeout = 0;
SET LOCAL client_encoding = 'UTF8';
SET LOCAL standard_conforming_strings = on;
SET LOCAL check_function_bodies = false;
SET LOCAL xmloption = content;
SET LOCAL client_min_messages = warning;
SET LOCAL row_security = off;

--
-- Name: public; Type: SCHEMA; Schema: -; Owner: -
--



--
-- Name: account_fields_json(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.account_fields_json(account bigint) RETURNS jsonb
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$
    SELECT coalesce(
        jsonb_agg(
            jsonb_build_object('name', f.name, 'value', f.value)
            || CASE WHEN f.verified_at IS NULL THEN '{}'::jsonb
                    ELSE jsonb_build_object(
                        'verified_at',
                        to_char(f.verified_at AT TIME ZONE 'UTC',
                                'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'))
               END
            ORDER BY f.position),
        '[]'::jsonb)
    FROM account_fields f
    WHERE f.account_id = account
$$;


--
-- Name: account_hidden(bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.account_hidden(viewer bigint, author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT viewer IS NOT NULL AND viewer <> author AND (
        EXISTS (SELECT 1 FROM blocks b
                WHERE (b.account_id = viewer AND b.target_account_id = author)
                   OR (b.account_id = author AND b.target_account_id = viewer))
        OR EXISTS (SELECT 1 FROM mutes m
                   WHERE m.account_id = viewer AND m.target_account_id = author
                     AND (m.expires_at IS NULL OR m.expires_at > now()))
        OR EXISTS (SELECT 1
                   FROM accounts a
                   JOIN account_domain_blocks adb
                     ON adb.account_id = viewer AND adb.domain = a.domain
                   WHERE a.id = author)
        OR EXISTS (SELECT 1
                   FROM accounts v
                   JOIN account_domain_blocks adb
                     ON adb.account_id = author AND adb.domain = v.domain
                   WHERE v.id = viewer)
    )
$$;


--
-- Name: account_silenced(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.account_silenced(author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1 FROM accounts a
        WHERE a.id = author
          AND (
            a.silenced_at IS NOT NULL
            OR (a.domain IS NOT NULL AND EXISTS (
                SELECT 1 FROM domain_blocks db
                WHERE db.domain = a.domain AND db.severity = 'silence'))
          )
    )
$$;


--
-- Name: instance_domain_allowed(text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.instance_domain_allowed(remote_domain text) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT remote_domain IS NULL OR (
        NOT EXISTS (
            SELECT 1 FROM domain_blocks db
            WHERE db.domain = remote_domain AND db.severity = 'suspend'
        )
        AND (
            NOT EXISTS (SELECT 1 FROM domain_allows)
            OR EXISTS (
                SELECT 1 FROM domain_allows da
                WHERE da.domain = remote_domain
            )
        )
    )
$$;


--
-- Name: instance_domain_rejects_media(text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.instance_domain_rejects_media(remote_domain text) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT remote_domain IS NOT NULL AND EXISTS (
        SELECT 1 FROM domain_blocks db
        WHERE db.domain = remote_domain AND db.reject_media
    )
$$;


--
-- Name: sender_filtered(bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.sender_filtered(recipient bigint, sender bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM blocks b
                   WHERE b.account_id = recipient AND b.target_account_id = sender)
        OR EXISTS (SELECT 1 FROM mutes m
                   WHERE m.account_id = recipient AND m.target_account_id = sender
                     AND m.hide_notifications
                     AND (m.expires_at IS NULL OR m.expires_at > now()))
        OR EXISTS (SELECT 1
                   FROM accounts a
                   JOIN account_domain_blocks adb
                     ON adb.account_id = recipient AND adb.domain = a.domain
                   WHERE a.id = sender)
$$;


--
-- Name: streaming_conversation_event(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.streaming_conversation_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM pg_notify('plamenu_streaming', json_build_object(
        'kind', 'conversation',
        'row_id', NEW.id,
        'account_id', NEW.account_id)::text);
    RETURN NULL;
END $$;


--
-- Name: streaming_notification_event(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.streaming_notification_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM pg_notify('plamenu_streaming', json_build_object(
        'kind', 'notification',
        'notification_id', NEW.id,
        'account_id', NEW.account_id)::text);
    RETURN NULL;
END $$;


--
-- Name: web_push_fanout(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.web_push_fanout() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.filtered THEN
        RETURN NULL;
    END IF;
    INSERT INTO push_delivery_jobs (subscription_id, notification_id)
    SELECT s.id, NEW.id
    FROM web_push_subscriptions s
    JOIN users u ON u.id = s.user_id
    JOIN web_push_alerts a
      ON a.subscription_id = s.id AND a.kind = NEW.kind AND a.enabled
    WHERE u.account_id = NEW.account_id
      AND CASE s.policy
            WHEN 'all' THEN true
            WHEN 'followed' THEN EXISTS (
                SELECT 1 FROM follows f
                WHERE f.account_id = NEW.account_id
                  AND f.target_account_id = NEW.from_account_id
                  AND NOT f.pending)
            WHEN 'follower' THEN EXISTS (
                SELECT 1 FROM follows f
                WHERE f.account_id = NEW.from_account_id
                  AND f.target_account_id = NEW.account_id
                  AND NOT f.pending)
            ELSE false
          END;
    RETURN NULL;
END $$;


SET LOCAL default_tablespace = '';

SET LOCAL default_table_access_method = heap;

--
-- Name: account_aliases; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_aliases (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    acct text NOT NULL,
    uri text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_archives; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_archives (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    state text DEFAULT 'scheduled'::text NOT NULL,
    file_name text,
    file_size bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    CONSTRAINT account_archives_state_check CHECK ((state = ANY (ARRAY['scheduled'::text, 'in_progress'::text, 'finished'::text])))
);


--
-- Name: account_archives_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.account_archives ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.account_archives_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: account_conversations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_conversations (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    conversation_id bigint NOT NULL,
    participant_account_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    status_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    last_status_id bigint,
    unread boolean DEFAULT false NOT NULL
);


--
-- Name: account_domain_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_domain_blocks (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    domain text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_endorsements; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_endorsements (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_fields; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_fields (
    account_id bigint NOT NULL,
    "position" integer NOT NULL,
    name text NOT NULL,
    value text NOT NULL,
    verified_at timestamp with time zone,
    CONSTRAINT account_fields_position_check CHECK ((("position" >= 0) AND ("position" < 20)))
);


--
-- Name: account_media_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_media_jobs (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    which text NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 0 NOT NULL
);


--
-- Name: account_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_migrations (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_acct text NOT NULL,
    target_account_id bigint,
    followers_count bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_moderation_notes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_moderation_notes (
    id bigint NOT NULL,
    content text NOT NULL,
    account_id bigint,
    target_account_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_notes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_notes (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    comment text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_statuses_cleanup_policies; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_statuses_cleanup_policies (
    account_id bigint NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    min_status_age integer DEFAULT 1209600 NOT NULL,
    keep_direct boolean DEFAULT true NOT NULL,
    keep_pinned boolean DEFAULT true NOT NULL,
    keep_polls boolean DEFAULT false NOT NULL,
    keep_media boolean DEFAULT false NOT NULL,
    keep_self_fav boolean DEFAULT true NOT NULL,
    keep_self_bookmark boolean DEFAULT true NOT NULL,
    min_favs integer,
    min_reblogs integer,
    last_inspected_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT account_statuses_cleanup_policies_min_favs_check CHECK ((min_favs >= 1)),
    CONSTRAINT account_statuses_cleanup_policies_min_reblogs_check CHECK ((min_reblogs >= 1))
);


--
-- Name: account_warning_presets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_warning_presets (
    id bigint NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: account_warnings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.account_warnings (
    id bigint NOT NULL,
    account_id bigint,
    target_account_id bigint NOT NULL,
    action text DEFAULT 'none'::text NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    report_id bigint,
    status_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    overruled_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: accounts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.accounts (
    id bigint NOT NULL,
    username text NOT NULL,
    domain text,
    display_name text DEFAULT ''::text NOT NULL,
    note text DEFAULT ''::text NOT NULL,
    private_key text,
    public_key text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    uri text,
    inbox_url text DEFAULT ''::text NOT NULL,
    shared_inbox_url text DEFAULT ''::text NOT NULL,
    public_key_id text,
    avatar_file_name text,
    header_file_name text,
    avatar_remote_url text,
    header_remote_url text,
    note_source text DEFAULT ''::text NOT NULL,
    featured_collection_url text,
    locked boolean DEFAULT false NOT NULL,
    also_known_as text[] DEFAULT '{}'::text[] NOT NULL,
    moved_to_uri text,
    url text,
    discoverable boolean DEFAULT true,
    feature_approval_policy integer DEFAULT 0 NOT NULL,
    is_bot boolean DEFAULT false NOT NULL,
    indexable boolean DEFAULT true NOT NULL,
    hide_collections boolean DEFAULT false NOT NULL,
    avatar_description text DEFAULT ''::text NOT NULL,
    header_description text DEFAULT ''::text NOT NULL,
    suspended_at timestamp with time zone,
    silenced_at timestamp with time zone,
    sensitized_at timestamp with time zone,
    suspension_origin text,
    avatar_file_size bigint,
    header_file_size bigint,
    followers_url text DEFAULT ''::text NOT NULL,
    following_url text DEFAULT ''::text NOT NULL,
    show_media boolean DEFAULT true NOT NULL,
    show_media_replies boolean DEFAULT true NOT NULL,
    show_featured boolean DEFAULT true NOT NULL,
    memorial boolean DEFAULT false NOT NULL,
    actor_type text,
    last_webfingered_at timestamp with time zone,
    ed25519_public_key text,
    ed25519_private_key text,
    deleted_at timestamp with time zone,
    attribution_domains text[] DEFAULT '{}'::text[] NOT NULL,
    outbox_url text DEFAULT ''::text NOT NULL,
    remote_statuses_count bigint
);


--
-- Name: admin_action_logs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.admin_action_logs (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    action text NOT NULL,
    target_type text NOT NULL,
    target_id bigint NOT NULL,
    human_identifier text DEFAULT ''::text NOT NULL,
    permalink text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: announcement_mutes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.announcement_mutes (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    announcement_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: announcement_reactions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.announcement_reactions (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    announcement_id bigint NOT NULL,
    name text DEFAULT ''::text NOT NULL,
    custom_emoji_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: announcements; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.announcements (
    id bigint NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    published boolean DEFAULT false NOT NULL,
    all_day boolean DEFAULT false NOT NULL,
    scheduled_at timestamp with time zone,
    starts_at timestamp with time zone,
    ends_at timestamp with time zone,
    published_at timestamp with time zone,
    notification_sent_at timestamp with time zone,
    status_ids bigint[],
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: appeals; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.appeals (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    account_warning_id bigint NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    approved_at timestamp with time zone,
    approved_by_account_id bigint,
    rejected_at timestamp with time zone,
    rejected_by_account_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.blocks (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: bookmarks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bookmarks (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: bulk_import_row_languages; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bulk_import_row_languages (
    bulk_import_row_id bigint NOT NULL,
    "position" integer NOT NULL,
    language text NOT NULL
);


--
-- Name: bulk_import_rows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bulk_import_rows (
    id bigint NOT NULL,
    bulk_import_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    row_position integer NOT NULL,
    acct text,
    show_reblogs boolean,
    notify boolean,
    hide_notifications boolean,
    domain text,
    uri text,
    list_name text
);


--
-- Name: bulk_import_rows_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.bulk_import_rows ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.bulk_import_rows_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: bulk_imports; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.bulk_imports (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    import_type text NOT NULL,
    state text DEFAULT 'unconfirmed'::text NOT NULL,
    overwrite boolean DEFAULT false NOT NULL,
    total_items integer DEFAULT 0 NOT NULL,
    processed_items integer DEFAULT 0 NOT NULL,
    imported_items integer DEFAULT 0 NOT NULL,
    original_filename text DEFAULT ''::text NOT NULL,
    likely_mismatched boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    finished_at timestamp with time zone,
    CONSTRAINT bulk_imports_counts_check CHECK (((total_items >= 0) AND (processed_items >= 0) AND (imported_items >= 0) AND (imported_items <= processed_items) AND (processed_items <= total_items))),
    CONSTRAINT bulk_imports_state_check CHECK ((state = ANY (ARRAY['unconfirmed'::text, 'scheduled'::text, 'in_progress'::text, 'finished'::text]))),
    CONSTRAINT bulk_imports_type_check CHECK ((import_type = ANY (ARRAY['following'::text, 'blocking'::text, 'muting'::text, 'domain_blocking'::text, 'bookmarks'::text, 'lists'::text])))
);


--
-- Name: bulk_imports_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.bulk_imports ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.bulk_imports_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: canonical_email_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.canonical_email_blocks (
    id bigint NOT NULL,
    canonical_email_hash text NOT NULL,
    reference_account_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: collection_items; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.collection_items (
    id bigint NOT NULL,
    collection_id bigint NOT NULL,
    account_id bigint,
    state text DEFAULT 'pending'::text NOT NULL,
    "position" integer DEFAULT 1 NOT NULL,
    uri text,
    object_uri text,
    activity_uri text,
    approval_uri text,
    approval_last_verified_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT collection_items_state_check CHECK ((state = ANY (ARRAY['pending'::text, 'accepted'::text, 'rejected'::text, 'revoked'::text])))
);


--
-- Name: collections; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.collections (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    name text NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    language text,
    sensitive boolean DEFAULT false NOT NULL,
    discoverable boolean DEFAULT false NOT NULL,
    local boolean NOT NULL,
    tag_id bigint,
    uri text,
    url text,
    original_number_of_items integer,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: conversation_mutes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.conversation_mutes (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    conversation_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: conversations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.conversations (
    id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    owner_account_id bigint,
    root_status_id bigint,
    uri text,
    history_uri text
);


--
-- Name: custom_emojis; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.custom_emojis (
    id bigint NOT NULL,
    shortcode text NOT NULL,
    domain text,
    uri text,
    image_remote_url text,
    image_file_name text,
    image_content_type text,
    disabled boolean DEFAULT false NOT NULL,
    visible_in_picker boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    image_file_size bigint,
    category text
);


--
-- Name: custom_filter_keywords; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.custom_filter_keywords (
    id bigint NOT NULL,
    custom_filter_id bigint NOT NULL,
    keyword text DEFAULT ''::text NOT NULL,
    whole_word boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: custom_filter_statuses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.custom_filter_statuses (
    id bigint NOT NULL,
    custom_filter_id bigint NOT NULL,
    status_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: custom_filters; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.custom_filters (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    action text DEFAULT 'warn'::text NOT NULL,
    context text[] DEFAULT '{}'::text[] NOT NULL,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT custom_filters_action_check CHECK ((action = ANY (ARRAY['warn'::text, 'hide'::text, 'blur'::text])))
);


--
-- Name: daily_interactions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.daily_interactions (
    day date NOT NULL,
    count bigint DEFAULT 0 NOT NULL
);


--
-- Name: delivery_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.delivery_jobs (
    id bigint NOT NULL,
    account_id bigint,
    inbox_url text NOT NULL,
    activity jsonb NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    synchronize_followers boolean DEFAULT false NOT NULL,
    activity_format_version smallint DEFAULT 1 NOT NULL,
    CONSTRAINT delivery_jobs_activity_format_version_check CHECK ((activity_format_version = 1)),
    CONSTRAINT delivery_jobs_activity_shape_check CHECK ((jsonb_typeof(activity) = 'object'::text))
);


--
-- Name: domain_allows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.domain_allows (
    id bigint NOT NULL,
    domain text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: domain_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.domain_blocks (
    id bigint NOT NULL,
    domain text NOT NULL,
    severity text DEFAULT 'silence'::text NOT NULL,
    reject_media boolean DEFAULT false NOT NULL,
    reject_reports boolean DEFAULT false NOT NULL,
    private_comment text,
    public_comment text,
    obfuscate boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT domain_blocks_severity_check CHECK ((severity = ANY (ARRAY['silence'::text, 'suspend'::text, 'noop'::text])))
);


--
-- Name: email_domain_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.email_domain_blocks (
    id bigint NOT NULL,
    domain text NOT NULL,
    parent_id bigint,
    allow_with_approval boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: email_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.email_jobs (
    id bigint NOT NULL,
    recipient text NOT NULL,
    subject text NOT NULL,
    body text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: email_jobs_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.email_jobs ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.email_jobs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: favourites; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.favourites (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: featured_tags; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.featured_tags (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    tag_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: follow_recommendation_mutes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.follow_recommendation_mutes (
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: follows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.follows (
    id bigint NOT NULL,
    uri text,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    pending boolean DEFAULT false NOT NULL,
    show_reblogs boolean DEFAULT true NOT NULL,
    notify boolean DEFAULT false NOT NULL,
    languages text[]
);


--
-- Name: group_affiliations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.group_affiliations (
    group_account_id bigint NOT NULL,
    account_id bigint NOT NULL,
    affiliation text NOT NULL,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: group_locked_posts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.group_locked_posts (
    group_account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: groups; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.groups (
    account_id bigint NOT NULL,
    membership_policy text DEFAULT 'open'::text NOT NULL,
    sensitive boolean DEFAULT false NOT NULL,
    created_by bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    posting_policy text DEFAULT 'members'::text NOT NULL
);


--
-- Name: host_reachability; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.host_reachability (
    host text NOT NULL,
    consecutive_failures integer DEFAULT 0 NOT NULL,
    first_failure_at timestamp with time zone,
    last_failure_at timestamp with time zone,
    last_failure_class text,
    last_error text,
    unreachable_since timestamp with time zone,
    last_success_at timestamp with time zone,
    next_probe_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    abandoned_at timestamp with time zone
);


--
-- Name: host_signature_prefs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.host_signature_prefs (
    host text NOT NULL,
    rfc9421 boolean NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: instance_actor_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.instance_actor_keys (
    id smallint DEFAULT 1 NOT NULL,
    private_key text NOT NULL,
    public_key text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    ed25519_public_key text,
    ed25519_private_key text,
    CONSTRAINT instance_actor_keys_id_check CHECK ((id = 1))
);


--
-- Name: instance_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.instance_settings (
    only_row boolean DEFAULT true NOT NULL,
    site_title text DEFAULT 'Plamenu'::text NOT NULL,
    site_short_description text DEFAULT ''::text NOT NULL,
    site_extended_description text DEFAULT ''::text NOT NULL,
    site_contact_username text DEFAULT ''::text NOT NULL,
    site_contact_email text DEFAULT ''::text NOT NULL,
    custom_css text DEFAULT ''::text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    registrations_mode text DEFAULT 'none'::text NOT NULL,
    rate_limiting_enabled boolean DEFAULT true NOT NULL,
    rate_limit_authenticated_api integer DEFAULT 1500 NOT NULL,
    rate_limit_per_token_api integer DEFAULT 300 NOT NULL,
    rate_limit_unauthenticated_api integer DEFAULT 300 NOT NULL,
    rate_limit_api_media integer DEFAULT 30 NOT NULL,
    rate_limit_api_delete integer DEFAULT 30 NOT NULL,
    rate_limit_api_sign_up integer DEFAULT 5 NOT NULL,
    rate_limit_app_registrations integer DEFAULT 5 NOT NULL,
    rate_limit_paging integer DEFAULT 300 NOT NULL,
    rate_limit_login_attempts integer DEFAULT 25 NOT NULL,
    rate_limit_password_resets integer DEFAULT 5 NOT NULL,
    rate_limit_sign_up_web integer DEFAULT 25 NOT NULL,
    max_characters integer DEFAULT 500 NOT NULL,
    max_media_attachments integer DEFAULT 6 NOT NULL,
    poll_max_options integer DEFAULT 6 NOT NULL,
    activity_api_enabled boolean DEFAULT true NOT NULL,
    peers_api_enabled boolean DEFAULT true NOT NULL,
    show_domain_blocks text DEFAULT 'disabled'::text NOT NULL,
    show_domain_blocks_rationale text DEFAULT 'disabled'::text NOT NULL,
    profile_directory boolean DEFAULT true NOT NULL,
    trends_enabled boolean DEFAULT true NOT NULL,
    trendable_by_default boolean DEFAULT false NOT NULL,
    self_destruct_initiated_at timestamp with time zone,
    media_cache_retention_days integer,
    min_age integer DEFAULT 0 NOT NULL,
    media_full_processing text DEFAULT 'passthrough'::text NOT NULL,
    media_preview_processing text DEFAULT 'avif'::text NOT NULL,
    media_remote_full_processing text DEFAULT 'passthrough'::text NOT NULL,
    media_cached_image_processing text DEFAULT 'avif'::text NOT NULL,
    group_creation_policy text DEFAULT 'everyone'::text NOT NULL,
    remote_video_max_mb integer DEFAULT 2048 NOT NULL,
    remote_video_max_height integer DEFAULT 1080 NOT NULL,
    CONSTRAINT instance_settings_media_cache_retention_days_check CHECK ((media_cache_retention_days >= 0)),
    CONSTRAINT instance_settings_only_row_check CHECK (only_row),
    CONSTRAINT instance_settings_registrations_mode_check CHECK ((registrations_mode = ANY (ARRAY['open'::text, 'approved'::text, 'none'::text]))),
    CONSTRAINT instance_settings_show_domain_blocks_check CHECK ((show_domain_blocks = ANY (ARRAY['disabled'::text, 'users'::text, 'all'::text]))),
    CONSTRAINT instance_settings_show_domain_blocks_rationale_check CHECK ((show_domain_blocks_rationale = ANY (ARRAY['disabled'::text, 'users'::text, 'all'::text])))
);


--
-- Name: invites; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.invites (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    code text NOT NULL,
    expires_at timestamp with time zone,
    max_uses integer,
    uses integer DEFAULT 0 NOT NULL,
    comment text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: ip_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ip_blocks (
    id bigint NOT NULL,
    ip text NOT NULL,
    severity text DEFAULT 'sign_up_block'::text NOT NULL,
    comment text DEFAULT ''::text NOT NULL,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT ip_blocks_severity_check CHECK ((severity = ANY (ARRAY['sign_up_requires_approval'::text, 'sign_up_block'::text, 'no_access'::text])))
);


--
-- Name: link_crawl_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.link_crawl_jobs (
    id bigint NOT NULL,
    status_id bigint NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: link_verification_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.link_verification_jobs (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: list_accounts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.list_accounts (
    id bigint NOT NULL,
    list_id bigint NOT NULL,
    account_id bigint NOT NULL,
    follow_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: lists; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.lists (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    title text NOT NULL,
    replies_policy text DEFAULT 'list'::text NOT NULL,
    exclusive boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT lists_replies_policy_check CHECK ((replies_policy = ANY (ARRAY['list'::text, 'followed'::text, 'none'::text])))
);


--
-- Name: login_activities; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.login_activities (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    ip text,
    authentication_method text,
    success boolean DEFAULT true NOT NULL,
    failure_reason text,
    user_agent text
);


--
-- Name: markers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.markers (
    user_id bigint NOT NULL,
    timeline text NOT NULL,
    last_read_id bigint DEFAULT 0 NOT NULL,
    version integer DEFAULT 0 NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: media_attachments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.media_attachments (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint,
    file_name text,
    remote_url text,
    content_type text NOT NULL,
    description text,
    width integer,
    height integer,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    kind text,
    blurhash text,
    focus_x double precision,
    focus_y double precision,
    small_file_name text,
    small_width integer,
    small_height integer,
    thumbnail_remote_url text,
    duration double precision,
    frame_rate text,
    bitrate bigint,
    processing text DEFAULT 'complete'::text NOT NULL,
    cached_at timestamp with time zone,
    scheduled_status_id bigint,
    file_size bigint,
    thumbnail_file_size bigint,
    remote_audio_url text,
    download_on_demand boolean DEFAULT false NOT NULL,
    last_served_at timestamp with time zone,
    hls_master_url text,
    CONSTRAINT media_attachments_check CHECK (((file_name IS NOT NULL) OR (remote_url IS NOT NULL))),
    CONSTRAINT media_attachments_kind_check CHECK (((kind IS NULL) OR (kind = ANY (ARRAY['image'::text, 'gifv'::text, 'video'::text, 'audio'::text])))),
    CONSTRAINT media_attachments_processing_check CHECK ((processing = ANY (ARRAY['queued'::text, 'complete'::text, 'failed'::text])))
);


--
-- Name: media_fetch_failures; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.media_fetch_failures (
    kind text NOT NULL,
    target_id bigint NOT NULL,
    failed_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 1 NOT NULL
);


--
-- Name: media_hls_segments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.media_hls_segments (
    id bigint NOT NULL,
    media_id bigint NOT NULL,
    origin_url text NOT NULL,
    range_start bigint NOT NULL,
    range_len bigint NOT NULL,
    cache_file text NOT NULL,
    bytes bigint NOT NULL,
    cached_at timestamp with time zone DEFAULT now() NOT NULL,
    last_served_at timestamp with time zone
);


--
-- Name: media_processing_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.media_processing_jobs (
    id bigint NOT NULL,
    media_id bigint NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 0 NOT NULL
);


--
-- Name: media_renditions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.media_renditions (
    id bigint NOT NULL,
    media_id bigint NOT NULL,
    height integer NOT NULL,
    width integer,
    frame_rate integer,
    size_bytes bigint,
    origin_url text NOT NULL,
    is_audio boolean DEFAULT false NOT NULL
);


--
-- Name: mutes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.mutes (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    hide_notifications boolean DEFAULT true NOT NULL,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: notification_permissions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification_permissions (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    from_account_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: notification_policies; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification_policies (
    account_id bigint NOT NULL,
    for_not_following smallint DEFAULT 0 NOT NULL,
    for_not_followers smallint DEFAULT 0 NOT NULL,
    for_new_accounts smallint DEFAULT 0 NOT NULL,
    for_private_mentions smallint DEFAULT 0 NOT NULL,
    for_limited_accounts smallint DEFAULT 1 NOT NULL,
    for_bots smallint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: notification_requests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notification_requests (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    from_account_id bigint NOT NULL,
    last_status_id bigint,
    notifications_count bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: notifications; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.notifications (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    from_account_id bigint NOT NULL,
    kind text NOT NULL,
    status_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    group_key text,
    collection_id bigint,
    filtered boolean DEFAULT false NOT NULL,
    emoji text
);


--
-- Name: oauth_apps; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_apps (
    id bigint NOT NULL,
    name text NOT NULL,
    website text,
    client_id text NOT NULL,
    client_secret_hash text NOT NULL,
    redirect_uris text[] NOT NULL,
    scopes text DEFAULT 'read'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: oauth_grants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_grants (
    id bigint NOT NULL,
    code_hash text NOT NULL,
    app_id bigint NOT NULL,
    user_id bigint NOT NULL,
    redirect_uri text NOT NULL,
    scopes text NOT NULL,
    pkce_challenge text,
    expires_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: oauth_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_tokens (
    id bigint NOT NULL,
    token_hash text NOT NULL,
    app_id bigint NOT NULL,
    user_id bigint,
    scopes text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    revoked_at timestamp with time zone,
    user_agent text,
    last_used_at timestamp with time zone,
    last_used_ip text
);


--
-- Name: otp_backup_codes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.otp_backup_codes (
    user_id bigint NOT NULL,
    code_hash text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: poll_votes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.poll_votes (
    id bigint NOT NULL,
    poll_id bigint NOT NULL,
    account_id bigint NOT NULL,
    choice integer NOT NULL,
    uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: polls; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.polls (
    id bigint NOT NULL,
    status_id bigint NOT NULL,
    account_id bigint NOT NULL,
    options text[] NOT NULL,
    cached_tallies bigint[] NOT NULL,
    multiple boolean DEFAULT false NOT NULL,
    hide_totals boolean DEFAULT false NOT NULL,
    voters_count bigint,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    expiry_processed_at timestamp with time zone
);


--
-- Name: preview_card_providers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.preview_card_providers (
    id bigint NOT NULL,
    domain text NOT NULL,
    trendable boolean,
    reviewed_at timestamp with time zone,
    requested_review_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: preview_card_trends; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.preview_card_trends (
    preview_card_id bigint NOT NULL,
    score double precision DEFAULT 0 NOT NULL,
    rank integer DEFAULT 0 NOT NULL,
    allowed boolean DEFAULT false NOT NULL,
    language text
);


--
-- Name: preview_card_usages; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.preview_card_usages (
    preview_card_id bigint NOT NULL,
    day date NOT NULL,
    account_id bigint NOT NULL,
    uses integer DEFAULT 0 NOT NULL
);


--
-- Name: preview_cards; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.preview_cards (
    id bigint NOT NULL,
    url text NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    kind text DEFAULT 'link'::text NOT NULL,
    author_name text DEFAULT ''::text NOT NULL,
    author_url text DEFAULT ''::text NOT NULL,
    provider_name text DEFAULT ''::text NOT NULL,
    provider_url text DEFAULT ''::text NOT NULL,
    html text DEFAULT ''::text NOT NULL,
    width integer DEFAULT 0 NOT NULL,
    height integer DEFAULT 0 NOT NULL,
    image_url text,
    image_description text DEFAULT ''::text NOT NULL,
    embed_url text DEFAULT ''::text NOT NULL,
    language text,
    published_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    image_file_size bigint,
    trendable boolean,
    max_score double precision,
    max_score_at timestamp with time zone,
    image_file_name text,
    image_content_type text,
    image_cached_at timestamp with time zone,
    author_account_id bigint
);


--
-- Name: preview_cards_statuses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.preview_cards_statuses (
    preview_card_id bigint NOT NULL,
    status_id bigint NOT NULL,
    url text DEFAULT ''::text NOT NULL
);


--
-- Name: push_delivery_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.push_delivery_jobs (
    id bigint NOT NULL,
    subscription_id bigint NOT NULL,
    notification_id bigint NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: push_delivery_jobs_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.push_delivery_jobs ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.push_delivery_jobs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: quote_verify_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.quote_verify_jobs (
    id bigint NOT NULL,
    quote_id bigint NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts integer DEFAULT 0 NOT NULL
);


--
-- Name: quotes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.quotes (
    id bigint NOT NULL,
    status_id bigint,
    status_uri text NOT NULL,
    account_id bigint NOT NULL,
    quoted_status_id bigint,
    quoted_account_id bigint,
    state text DEFAULT 'pending'::text NOT NULL,
    activity_uri text,
    approval_uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    quoted_uri text,
    legacy boolean DEFAULT false NOT NULL,
    CONSTRAINT quotes_state_check CHECK ((state = ANY (ARRAY['pending'::text, 'accepted'::text, 'rejected'::text, 'revoked'::text])))
);


--
-- Name: relays; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.relays (
    id bigint NOT NULL,
    inbox_url text NOT NULL,
    state text DEFAULT 'idle'::text NOT NULL,
    follow_activity_id text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT relays_state_check CHECK ((state = ANY (ARRAY['idle'::text, 'pending'::text, 'accepted'::text, 'rejected'::text])))
);


--
-- Name: remote_fetch_failures; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.remote_fetch_failures (
    scope text NOT NULL,
    failure_key text NOT NULL,
    attempts integer DEFAULT 1 NOT NULL,
    retry_at timestamp with time zone,
    abandoned_at timestamp with time zone,
    last_error text,
    first_failed_at timestamp with time zone DEFAULT now() NOT NULL,
    last_failed_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: reply_fetch_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.reply_fetch_jobs (
    id bigint NOT NULL,
    status_id bigint NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: report_notes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.report_notes (
    id bigint NOT NULL,
    content text NOT NULL,
    report_id bigint NOT NULL,
    account_id bigint,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: reports; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.reports (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    target_account_id bigint NOT NULL,
    status_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    comment text DEFAULT ''::text NOT NULL,
    category text DEFAULT 'other'::text NOT NULL,
    forwarded boolean,
    rule_ids bigint[],
    uri text,
    action_taken_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    assigned_account_id bigint,
    action_taken_by_account_id bigint,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    group_account_id bigint
);


--
-- Name: rule_translations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.rule_translations (
    id bigint NOT NULL,
    rule_id bigint NOT NULL,
    language text NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    hint text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: rules; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.rules (
    id bigint NOT NULL,
    priority integer DEFAULT 0 NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    hint text DEFAULT ''::text NOT NULL,
    deleted_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: scheduled_statuses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.scheduled_statuses (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    scheduled_at timestamp with time zone NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    visibility text NOT NULL,
    in_reply_to_id bigint,
    quoted_status_id bigint,
    spoiler_text text DEFAULT ''::text NOT NULL,
    sensitive boolean DEFAULT false NOT NULL,
    language text,
    application_id bigint,
    media_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    poll_options text[],
    poll_expires_in bigint,
    poll_multiple boolean DEFAULT false NOT NULL,
    poll_hide_totals boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    quote_approval_policy integer,
    content_type text DEFAULT 'text/plain'::text NOT NULL,
    CONSTRAINT scheduled_statuses_visibility_check CHECK ((visibility = ANY (ARRAY['public'::text, 'unlisted'::text, 'private'::text, 'direct'::text, 'local'::text])))
);


--
-- Name: site_upload_variants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.site_upload_variants (
    var text NOT NULL,
    style text NOT NULL,
    file_name text NOT NULL,
    width integer NOT NULL,
    height integer NOT NULL
);


--
-- Name: site_uploads; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.site_uploads (
    var text NOT NULL,
    file_name text NOT NULL,
    content_type text NOT NULL,
    file_size bigint NOT NULL,
    width integer NOT NULL,
    height integer NOT NULL,
    blurhash text,
    description text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT site_uploads_var_check CHECK ((var = ANY (ARRAY['thumbnail'::text, 'mascot'::text, 'favicon'::text, 'app_icon'::text])))
);


--
-- Name: software_updates; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.software_updates (
    id bigint NOT NULL,
    version text NOT NULL,
    urgent boolean DEFAULT false NOT NULL,
    release_type text DEFAULT 'patch'::text NOT NULL,
    release_notes text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: software_updates_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.software_updates ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.software_updates_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: status_conversations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_conversations (
    status_id bigint NOT NULL,
    conversation_id bigint NOT NULL
);


--
-- Name: status_dislikes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_dislikes (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: status_edits; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_edits (
    id bigint NOT NULL,
    status_id bigint NOT NULL,
    account_id bigint NOT NULL,
    content text DEFAULT ''::text NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    spoiler_text text DEFAULT ''::text NOT NULL,
    sensitive boolean DEFAULT false NOT NULL,
    media_ids bigint[] DEFAULT '{}'::bigint[] NOT NULL,
    created_at timestamp with time zone NOT NULL
);


--
-- Name: status_events; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_events (
    status_id bigint NOT NULL,
    start_time timestamp with time zone,
    end_time timestamp with time zone,
    location_name text,
    timezone text,
    event_status text
);


--
-- Name: status_mentions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_mentions (
    status_id bigint NOT NULL,
    account_id bigint NOT NULL,
    silent boolean DEFAULT false NOT NULL
);


--
-- Name: status_pins; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_pins (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: status_reactions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_reactions (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    status_id bigint NOT NULL,
    name text NOT NULL,
    custom_emoji_url text,
    uri text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: status_reply_fetches; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_reply_fetches (
    status_id bigint NOT NULL,
    fetched_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: status_tags; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_tags (
    status_id bigint NOT NULL,
    tag_id bigint NOT NULL,
    sort_at timestamp with time zone NOT NULL
);


--
-- Name: status_tombstones; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_tombstones (
    status_id bigint NOT NULL,
    account_id bigint,
    uri text NOT NULL,
    deleted_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: status_trends; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.status_trends (
    status_id bigint NOT NULL,
    account_id bigint NOT NULL,
    score double precision DEFAULT 0 NOT NULL,
    rank integer DEFAULT 0 NOT NULL,
    allowed boolean DEFAULT false NOT NULL,
    language text
);


--
-- Name: statuses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.statuses (
    id bigint NOT NULL,
    uri text,
    account_id bigint NOT NULL,
    content text DEFAULT ''::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    visibility text DEFAULT 'public'::text NOT NULL,
    in_reply_to_id bigint,
    reblog_of_id bigint,
    edited_at timestamp with time zone,
    spoiler_text text DEFAULT ''::text NOT NULL,
    sensitive boolean DEFAULT false NOT NULL,
    language text,
    text text DEFAULT ''::text NOT NULL,
    url text,
    quote_approval_policy integer DEFAULT 0 NOT NULL,
    application_id bigint,
    sort_at timestamp with time zone DEFAULT now() NOT NULL,
    trendable boolean,
    content_type text DEFAULT 'text/plain'::text NOT NULL,
    title text,
    object_type text,
    external_url text,
    in_reply_to_uri text,
    CONSTRAINT statuses_visibility_check CHECK ((visibility = ANY (ARRAY['public'::text, 'unlisted'::text, 'private'::text, 'direct'::text, 'local'::text])))
);


--
-- Name: tag_follows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tag_follows (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    tag_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: tag_trends; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tag_trends (
    tag_id bigint NOT NULL,
    score double precision DEFAULT 0 NOT NULL,
    rank integer DEFAULT 0 NOT NULL,
    allowed boolean DEFAULT false NOT NULL,
    language text DEFAULT ''::text NOT NULL
);


--
-- Name: tag_usages; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tag_usages (
    tag_id bigint NOT NULL,
    day date NOT NULL,
    account_id bigint NOT NULL,
    uses integer DEFAULT 0 NOT NULL
);


--
-- Name: tagged_objects; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tagged_objects (
    id bigint NOT NULL,
    status_id bigint NOT NULL,
    collection_id bigint NOT NULL,
    uri text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: tags; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tags (
    id bigint NOT NULL,
    name text NOT NULL,
    display_name text,
    usable boolean,
    listable boolean,
    trendable boolean,
    reviewed_at timestamp with time zone,
    requested_review_at timestamp with time zone,
    max_score double precision,
    max_score_at timestamp with time zone
);


--
-- Name: terms_of_services; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.terms_of_services (
    id bigint NOT NULL,
    text text DEFAULT ''::text NOT NULL,
    changelog text DEFAULT ''::text NOT NULL,
    published_at timestamp with time zone,
    effective_date date,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: two_factor_challenges; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.two_factor_challenges (
    id bigint NOT NULL,
    token_hash text NOT NULL,
    user_id bigint NOT NULL,
    context text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    webauthn_state jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    webauthn_state_format_version smallint DEFAULT 1 NOT NULL,
    CONSTRAINT two_factor_challenges_webauthn_state_format_version_check CHECK ((webauthn_state_format_version = 1)),
    CONSTRAINT two_factor_challenges_webauthn_state_shape_check CHECK (((webauthn_state IS NULL) OR (jsonb_typeof(webauthn_state) = 'object'::text)))
);


--
-- Name: user_roles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_roles (
    id bigint NOT NULL,
    name text NOT NULL,
    color text DEFAULT ''::text NOT NULL,
    "position" integer DEFAULT 0 NOT NULL,
    permissions bigint DEFAULT 0 NOT NULL,
    highlighted boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: username_blocks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.username_blocks (
    id bigint NOT NULL,
    username text NOT NULL,
    normalized_username text NOT NULL,
    exact boolean DEFAULT false NOT NULL,
    allow_with_approval boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.users (
    id bigint NOT NULL,
    account_id bigint NOT NULL,
    email text,
    password_hash text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    posting_default_visibility text DEFAULT 'default'::text NOT NULL,
    posting_default_sensitive boolean DEFAULT false NOT NULL,
    posting_default_language text DEFAULT 'en'::text NOT NULL,
    reading_expand_media text DEFAULT 'default'::text NOT NULL,
    reading_expand_spoilers boolean DEFAULT false NOT NULL,
    reading_autoplay_gifs boolean DEFAULT false NOT NULL,
    show_application boolean DEFAULT true NOT NULL,
    role_id bigint,
    approved boolean DEFAULT true NOT NULL,
    confirmed_at timestamp with time zone,
    disabled boolean DEFAULT false NOT NULL,
    current_sign_in_at timestamp with time zone,
    last_sign_in_at timestamp with time zone,
    sign_in_count integer DEFAULT 0 NOT NULL,
    locale text,
    created_by_application_id bigint,
    timeline_order text DEFAULT 'published'::text NOT NULL,
    confirmation_token_hash text,
    confirmation_sent_at timestamp with time zone,
    sign_up_ip text,
    invite_request_text text,
    reset_password_token_hash text,
    reset_password_sent_at timestamp with time zone,
    invite_id bigint,
    current_sign_in_ip text,
    last_sign_in_ip text,
    failed_attempts integer DEFAULT 0 NOT NULL,
    locked_at timestamp with time zone,
    otp_secret text,
    otp_required_for_login boolean DEFAULT false NOT NULL,
    otp_consumed_timestep bigint,
    webauthn_id text,
    posting_default_quote_policy text DEFAULT 'public'::text NOT NULL,
    posting_languages text[],
    thread_order text DEFAULT 'tree'::text NOT NULL,
    noindex boolean DEFAULT false NOT NULL,
    chosen_languages text[],
    time_zone text,
    age_verified_at timestamp with time zone,
    sign_in_token text,
    sign_in_token_sent_at timestamp with time zone,
    new_ip_sign_in_alert boolean DEFAULT false NOT NULL,
    reading_allow_direct_remote_media boolean DEFAULT false NOT NULL,
    posting_default_content_type text DEFAULT 'text/plain'::text NOT NULL,
    CONSTRAINT users_posting_default_content_type_check CHECK ((posting_default_content_type = ANY (ARRAY['text/plain'::text, 'text/markdown'::text, 'text/html'::text]))),
    CONSTRAINT users_posting_default_language_check CHECK ((btrim(posting_default_language) <> ''::text)),
    CONSTRAINT users_posting_default_visibility_check CHECK ((posting_default_visibility = ANY (ARRAY['default'::text, 'public'::text, 'unlisted'::text, 'private'::text, 'direct'::text, 'local'::text]))),
    CONSTRAINT users_reading_expand_media_check CHECK ((reading_expand_media = ANY (ARRAY['default'::text, 'show_all'::text, 'hide_all'::text]))),
    CONSTRAINT users_timeline_order_check CHECK ((timeline_order = ANY (ARRAY['published'::text, 'received'::text])))
);


--
-- Name: vapid_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.vapid_keys (
    id smallint DEFAULT 1 NOT NULL,
    private_key text NOT NULL,
    public_key text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT vapid_keys_id_check CHECK ((id = 1))
);


--
-- Name: web_push_alerts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.web_push_alerts (
    subscription_id bigint NOT NULL,
    kind text NOT NULL,
    enabled boolean NOT NULL
);


--
-- Name: web_push_subscriptions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.web_push_subscriptions (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    access_token_id bigint NOT NULL,
    access_token text NOT NULL,
    endpoint text NOT NULL,
    key_p256dh text NOT NULL,
    key_auth text NOT NULL,
    standard boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    policy text DEFAULT 'all'::text NOT NULL
);


--
-- Name: web_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.web_settings (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    data_format_version smallint DEFAULT 1 NOT NULL,
    CONSTRAINT web_settings_data_format_version_check CHECK ((data_format_version = 1)),
    CONSTRAINT web_settings_data_shape_check CHECK ((jsonb_typeof(data) = 'object'::text))
);


--
-- Name: webauthn_credentials; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.webauthn_credentials (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    external_id text NOT NULL,
    nickname text NOT NULL,
    credential jsonb NOT NULL,
    sign_count bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    credential_format_version smallint DEFAULT 1 NOT NULL,
    CONSTRAINT webauthn_credentials_credential_format_version_check CHECK ((credential_format_version = 1)),
    CONSTRAINT webauthn_credentials_credential_shape_check CHECK ((jsonb_typeof(credential) = 'object'::text))
);


--
-- Name: webhook_delivery_jobs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.webhook_delivery_jobs (
    id bigint NOT NULL,
    webhook_id bigint NOT NULL,
    event text NOT NULL,
    body text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: webhook_delivery_jobs_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.webhook_delivery_jobs ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.webhook_delivery_jobs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: webhooks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.webhooks (
    id bigint NOT NULL,
    url text NOT NULL,
    events text[] DEFAULT '{}'::text[] NOT NULL,
    secret text DEFAULT ''::text NOT NULL,
    template text,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT webhooks_events_not_empty CHECK ((COALESCE(array_length(events, 1), 0) > 0))
);


--
-- Name: account_aliases account_aliases_account_id_uri_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_aliases
    ADD CONSTRAINT account_aliases_account_id_uri_key UNIQUE (account_id, uri);


--
-- Name: account_aliases account_aliases_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_aliases
    ADD CONSTRAINT account_aliases_pkey PRIMARY KEY (id);


--
-- Name: account_archives account_archives_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_archives
    ADD CONSTRAINT account_archives_pkey PRIMARY KEY (id);


--
-- Name: account_conversations account_conversations_account_id_conversation_id_participan_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_conversations
    ADD CONSTRAINT account_conversations_account_id_conversation_id_participan_key UNIQUE (account_id, conversation_id, participant_account_ids);


--
-- Name: account_conversations account_conversations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_conversations
    ADD CONSTRAINT account_conversations_pkey PRIMARY KEY (id);


--
-- Name: account_domain_blocks account_domain_blocks_account_id_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_domain_blocks
    ADD CONSTRAINT account_domain_blocks_account_id_domain_key UNIQUE (account_id, domain);


--
-- Name: account_domain_blocks account_domain_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_domain_blocks
    ADD CONSTRAINT account_domain_blocks_pkey PRIMARY KEY (id);


--
-- Name: account_endorsements account_endorsements_account_id_target_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_endorsements
    ADD CONSTRAINT account_endorsements_account_id_target_account_id_key UNIQUE (account_id, target_account_id);


--
-- Name: account_endorsements account_endorsements_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_endorsements
    ADD CONSTRAINT account_endorsements_pkey PRIMARY KEY (id);


--
-- Name: account_fields account_fields_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_fields
    ADD CONSTRAINT account_fields_pkey PRIMARY KEY (account_id, "position");


--
-- Name: account_media_jobs account_media_jobs_account_id_which_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_media_jobs
    ADD CONSTRAINT account_media_jobs_account_id_which_key UNIQUE (account_id, which);


--
-- Name: account_media_jobs account_media_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_media_jobs
    ADD CONSTRAINT account_media_jobs_pkey PRIMARY KEY (id);


--
-- Name: account_migrations account_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_migrations
    ADD CONSTRAINT account_migrations_pkey PRIMARY KEY (id);


--
-- Name: account_moderation_notes account_moderation_notes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_moderation_notes
    ADD CONSTRAINT account_moderation_notes_pkey PRIMARY KEY (id);


--
-- Name: account_notes account_notes_account_id_target_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_notes
    ADD CONSTRAINT account_notes_account_id_target_account_id_key UNIQUE (account_id, target_account_id);


--
-- Name: account_notes account_notes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_notes
    ADD CONSTRAINT account_notes_pkey PRIMARY KEY (id);


--
-- Name: account_statuses_cleanup_policies account_statuses_cleanup_policies_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_statuses_cleanup_policies
    ADD CONSTRAINT account_statuses_cleanup_policies_pkey PRIMARY KEY (account_id);


--
-- Name: account_warning_presets account_warning_presets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_warning_presets
    ADD CONSTRAINT account_warning_presets_pkey PRIMARY KEY (id);


--
-- Name: account_warnings account_warnings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_warnings
    ADD CONSTRAINT account_warnings_pkey PRIMARY KEY (id);


--
-- Name: accounts accounts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.accounts
    ADD CONSTRAINT accounts_pkey PRIMARY KEY (id);


--
-- Name: admin_action_logs admin_action_logs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.admin_action_logs
    ADD CONSTRAINT admin_action_logs_pkey PRIMARY KEY (id);


--
-- Name: announcement_mutes announcement_mutes_account_id_announcement_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_mutes
    ADD CONSTRAINT announcement_mutes_account_id_announcement_id_key UNIQUE (account_id, announcement_id);


--
-- Name: announcement_mutes announcement_mutes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_mutes
    ADD CONSTRAINT announcement_mutes_pkey PRIMARY KEY (id);


--
-- Name: announcement_reactions announcement_reactions_account_id_announcement_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_reactions
    ADD CONSTRAINT announcement_reactions_account_id_announcement_id_name_key UNIQUE (account_id, announcement_id, name);


--
-- Name: announcement_reactions announcement_reactions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_reactions
    ADD CONSTRAINT announcement_reactions_pkey PRIMARY KEY (id);


--
-- Name: announcements announcements_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcements
    ADD CONSTRAINT announcements_pkey PRIMARY KEY (id);


--
-- Name: appeals appeals_account_warning_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_account_warning_id_key UNIQUE (account_warning_id);


--
-- Name: appeals appeals_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_pkey PRIMARY KEY (id);


--
-- Name: blocks blocks_account_id_target_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.blocks
    ADD CONSTRAINT blocks_account_id_target_account_id_key UNIQUE (account_id, target_account_id);


--
-- Name: blocks blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.blocks
    ADD CONSTRAINT blocks_pkey PRIMARY KEY (id);


--
-- Name: bookmarks bookmarks_account_id_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bookmarks
    ADD CONSTRAINT bookmarks_account_id_status_id_key UNIQUE (account_id, status_id);


--
-- Name: bookmarks bookmarks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bookmarks
    ADD CONSTRAINT bookmarks_pkey PRIMARY KEY (id);


--
-- Name: bulk_import_row_languages bulk_import_row_languages_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_import_row_languages
    ADD CONSTRAINT bulk_import_row_languages_pkey PRIMARY KEY (bulk_import_row_id, "position");


--
-- Name: bulk_import_rows bulk_import_rows_import_position_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_import_rows
    ADD CONSTRAINT bulk_import_rows_import_position_key UNIQUE (bulk_import_id, row_position);


--
-- Name: bulk_import_rows bulk_import_rows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_import_rows
    ADD CONSTRAINT bulk_import_rows_pkey PRIMARY KEY (id);


--
-- Name: bulk_imports bulk_imports_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_imports
    ADD CONSTRAINT bulk_imports_pkey PRIMARY KEY (id);


--
-- Name: canonical_email_blocks canonical_email_blocks_canonical_email_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.canonical_email_blocks
    ADD CONSTRAINT canonical_email_blocks_canonical_email_hash_key UNIQUE (canonical_email_hash);


--
-- Name: canonical_email_blocks canonical_email_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.canonical_email_blocks
    ADD CONSTRAINT canonical_email_blocks_pkey PRIMARY KEY (id);


--
-- Name: collection_items collection_items_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collection_items
    ADD CONSTRAINT collection_items_pkey PRIMARY KEY (id);


--
-- Name: collections collections_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_pkey PRIMARY KEY (id);


--
-- Name: collections collections_uri_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_uri_key UNIQUE (uri);


--
-- Name: conversation_mutes conversation_mutes_account_id_conversation_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_mutes
    ADD CONSTRAINT conversation_mutes_account_id_conversation_id_key UNIQUE (account_id, conversation_id);


--
-- Name: conversation_mutes conversation_mutes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_mutes
    ADD CONSTRAINT conversation_mutes_pkey PRIMARY KEY (id);


--
-- Name: conversations conversations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversations
    ADD CONSTRAINT conversations_pkey PRIMARY KEY (id);


--
-- Name: custom_emojis custom_emojis_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_emojis
    ADD CONSTRAINT custom_emojis_pkey PRIMARY KEY (id);


--
-- Name: custom_emojis custom_emojis_shortcode_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_emojis
    ADD CONSTRAINT custom_emojis_shortcode_domain_key UNIQUE NULLS NOT DISTINCT (shortcode, domain);


--
-- Name: custom_filter_keywords custom_filter_keywords_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_keywords
    ADD CONSTRAINT custom_filter_keywords_pkey PRIMARY KEY (id);


--
-- Name: custom_filter_statuses custom_filter_statuses_custom_filter_id_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_statuses
    ADD CONSTRAINT custom_filter_statuses_custom_filter_id_status_id_key UNIQUE (custom_filter_id, status_id);


--
-- Name: custom_filter_statuses custom_filter_statuses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_statuses
    ADD CONSTRAINT custom_filter_statuses_pkey PRIMARY KEY (id);


--
-- Name: custom_filters custom_filters_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filters
    ADD CONSTRAINT custom_filters_pkey PRIMARY KEY (id);


--
-- Name: daily_interactions daily_interactions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.daily_interactions
    ADD CONSTRAINT daily_interactions_pkey PRIMARY KEY (day);


--
-- Name: delivery_jobs delivery_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_jobs
    ADD CONSTRAINT delivery_jobs_pkey PRIMARY KEY (id);


--
-- Name: domain_allows domain_allows_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.domain_allows
    ADD CONSTRAINT domain_allows_domain_key UNIQUE (domain);


--
-- Name: domain_allows domain_allows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.domain_allows
    ADD CONSTRAINT domain_allows_pkey PRIMARY KEY (id);


--
-- Name: domain_blocks domain_blocks_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.domain_blocks
    ADD CONSTRAINT domain_blocks_domain_key UNIQUE (domain);


--
-- Name: domain_blocks domain_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.domain_blocks
    ADD CONSTRAINT domain_blocks_pkey PRIMARY KEY (id);


--
-- Name: email_domain_blocks email_domain_blocks_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email_domain_blocks
    ADD CONSTRAINT email_domain_blocks_domain_key UNIQUE (domain);


--
-- Name: email_domain_blocks email_domain_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email_domain_blocks
    ADD CONSTRAINT email_domain_blocks_pkey PRIMARY KEY (id);


--
-- Name: email_jobs email_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email_jobs
    ADD CONSTRAINT email_jobs_pkey PRIMARY KEY (id);


--
-- Name: favourites favourites_account_id_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favourites
    ADD CONSTRAINT favourites_account_id_status_id_key UNIQUE (account_id, status_id);


--
-- Name: favourites favourites_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favourites
    ADD CONSTRAINT favourites_pkey PRIMARY KEY (id);


--
-- Name: featured_tags featured_tags_account_id_tag_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.featured_tags
    ADD CONSTRAINT featured_tags_account_id_tag_id_key UNIQUE (account_id, tag_id);


--
-- Name: featured_tags featured_tags_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.featured_tags
    ADD CONSTRAINT featured_tags_pkey PRIMARY KEY (id);


--
-- Name: follow_recommendation_mutes follow_recommendation_mutes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follow_recommendation_mutes
    ADD CONSTRAINT follow_recommendation_mutes_pkey PRIMARY KEY (account_id, target_account_id);


--
-- Name: follows follows_account_id_target_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follows
    ADD CONSTRAINT follows_account_id_target_account_id_key UNIQUE (account_id, target_account_id);


--
-- Name: follows follows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follows
    ADD CONSTRAINT follows_pkey PRIMARY KEY (id);


--
-- Name: group_affiliations group_affiliations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_affiliations
    ADD CONSTRAINT group_affiliations_pkey PRIMARY KEY (group_account_id, account_id);


--
-- Name: group_locked_posts group_locked_posts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_locked_posts
    ADD CONSTRAINT group_locked_posts_pkey PRIMARY KEY (group_account_id, status_id);


--
-- Name: groups groups_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.groups
    ADD CONSTRAINT groups_pkey PRIMARY KEY (account_id);


--
-- Name: host_reachability host_reachability_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.host_reachability
    ADD CONSTRAINT host_reachability_pkey PRIMARY KEY (host);


--
-- Name: host_signature_prefs host_signature_prefs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.host_signature_prefs
    ADD CONSTRAINT host_signature_prefs_pkey PRIMARY KEY (host);


--
-- Name: instance_actor_keys instance_actor_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.instance_actor_keys
    ADD CONSTRAINT instance_actor_keys_pkey PRIMARY KEY (id);


--
-- Name: instance_settings instance_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.instance_settings
    ADD CONSTRAINT instance_settings_pkey PRIMARY KEY (only_row);


--
-- Name: invites invites_code_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.invites
    ADD CONSTRAINT invites_code_key UNIQUE (code);


--
-- Name: invites invites_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.invites
    ADD CONSTRAINT invites_pkey PRIMARY KEY (id);


--
-- Name: ip_blocks ip_blocks_ip_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ip_blocks
    ADD CONSTRAINT ip_blocks_ip_key UNIQUE (ip);


--
-- Name: ip_blocks ip_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ip_blocks
    ADD CONSTRAINT ip_blocks_pkey PRIMARY KEY (id);


--
-- Name: link_crawl_jobs link_crawl_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.link_crawl_jobs
    ADD CONSTRAINT link_crawl_jobs_pkey PRIMARY KEY (id);


--
-- Name: link_verification_jobs link_verification_jobs_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.link_verification_jobs
    ADD CONSTRAINT link_verification_jobs_account_id_key UNIQUE (account_id);


--
-- Name: link_verification_jobs link_verification_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.link_verification_jobs
    ADD CONSTRAINT link_verification_jobs_pkey PRIMARY KEY (id);


--
-- Name: list_accounts list_accounts_list_id_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.list_accounts
    ADD CONSTRAINT list_accounts_list_id_account_id_key UNIQUE (list_id, account_id);


--
-- Name: list_accounts list_accounts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.list_accounts
    ADD CONSTRAINT list_accounts_pkey PRIMARY KEY (id);


--
-- Name: lists lists_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.lists
    ADD CONSTRAINT lists_pkey PRIMARY KEY (id);


--
-- Name: login_activities login_activities_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_activities
    ADD CONSTRAINT login_activities_pkey PRIMARY KEY (id);


--
-- Name: markers markers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.markers
    ADD CONSTRAINT markers_pkey PRIMARY KEY (user_id, timeline);


--
-- Name: media_attachments media_attachments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_attachments
    ADD CONSTRAINT media_attachments_pkey PRIMARY KEY (id);


--
-- Name: media_fetch_failures media_fetch_failures_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_fetch_failures
    ADD CONSTRAINT media_fetch_failures_pkey PRIMARY KEY (kind, target_id);


--
-- Name: media_hls_segments media_hls_segments_origin_url_range_start_range_len_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_hls_segments
    ADD CONSTRAINT media_hls_segments_origin_url_range_start_range_len_key UNIQUE (origin_url, range_start, range_len);


--
-- Name: media_hls_segments media_hls_segments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_hls_segments
    ADD CONSTRAINT media_hls_segments_pkey PRIMARY KEY (id);


--
-- Name: media_processing_jobs media_processing_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_processing_jobs
    ADD CONSTRAINT media_processing_jobs_pkey PRIMARY KEY (id);


--
-- Name: media_renditions media_renditions_media_id_origin_url_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_renditions
    ADD CONSTRAINT media_renditions_media_id_origin_url_key UNIQUE (media_id, origin_url);


--
-- Name: media_renditions media_renditions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_renditions
    ADD CONSTRAINT media_renditions_pkey PRIMARY KEY (id);


--
-- Name: mutes mutes_account_id_target_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mutes
    ADD CONSTRAINT mutes_account_id_target_account_id_key UNIQUE (account_id, target_account_id);


--
-- Name: mutes mutes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mutes
    ADD CONSTRAINT mutes_pkey PRIMARY KEY (id);


--
-- Name: notification_permissions notification_permissions_account_id_from_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_permissions
    ADD CONSTRAINT notification_permissions_account_id_from_account_id_key UNIQUE (account_id, from_account_id);


--
-- Name: notification_permissions notification_permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_permissions
    ADD CONSTRAINT notification_permissions_pkey PRIMARY KEY (id);


--
-- Name: notification_policies notification_policies_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_policies
    ADD CONSTRAINT notification_policies_pkey PRIMARY KEY (account_id);


--
-- Name: notification_requests notification_requests_account_id_from_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_requests
    ADD CONSTRAINT notification_requests_account_id_from_account_id_key UNIQUE (account_id, from_account_id);


--
-- Name: notification_requests notification_requests_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_requests
    ADD CONSTRAINT notification_requests_pkey PRIMARY KEY (id);


--
-- Name: notifications notifications_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_pkey PRIMARY KEY (id);


--
-- Name: oauth_apps oauth_apps_client_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_apps
    ADD CONSTRAINT oauth_apps_client_id_key UNIQUE (client_id);


--
-- Name: oauth_apps oauth_apps_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_apps
    ADD CONSTRAINT oauth_apps_pkey PRIMARY KEY (id);


--
-- Name: oauth_grants oauth_grants_code_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_grants
    ADD CONSTRAINT oauth_grants_code_hash_key UNIQUE (code_hash);


--
-- Name: oauth_grants oauth_grants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_grants
    ADD CONSTRAINT oauth_grants_pkey PRIMARY KEY (id);


--
-- Name: oauth_tokens oauth_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT oauth_tokens_pkey PRIMARY KEY (id);


--
-- Name: oauth_tokens oauth_tokens_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT oauth_tokens_token_hash_key UNIQUE (token_hash);


--
-- Name: otp_backup_codes otp_backup_codes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.otp_backup_codes
    ADD CONSTRAINT otp_backup_codes_pkey PRIMARY KEY (user_id, code_hash);


--
-- Name: poll_votes poll_votes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.poll_votes
    ADD CONSTRAINT poll_votes_pkey PRIMARY KEY (id);


--
-- Name: poll_votes poll_votes_poll_id_account_id_choice_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.poll_votes
    ADD CONSTRAINT poll_votes_poll_id_account_id_choice_key UNIQUE (poll_id, account_id, choice);


--
-- Name: polls polls_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.polls
    ADD CONSTRAINT polls_pkey PRIMARY KEY (id);


--
-- Name: polls polls_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.polls
    ADD CONSTRAINT polls_status_id_key UNIQUE (status_id);


--
-- Name: preview_card_providers preview_card_providers_domain_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_providers
    ADD CONSTRAINT preview_card_providers_domain_key UNIQUE (domain);


--
-- Name: preview_card_providers preview_card_providers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_providers
    ADD CONSTRAINT preview_card_providers_pkey PRIMARY KEY (id);


--
-- Name: preview_card_trends preview_card_trends_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_trends
    ADD CONSTRAINT preview_card_trends_pkey PRIMARY KEY (preview_card_id);


--
-- Name: preview_card_usages preview_card_usages_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_usages
    ADD CONSTRAINT preview_card_usages_pkey PRIMARY KEY (preview_card_id, day, account_id);


--
-- Name: preview_cards preview_cards_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards
    ADD CONSTRAINT preview_cards_pkey PRIMARY KEY (id);


--
-- Name: preview_cards_statuses preview_cards_statuses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards_statuses
    ADD CONSTRAINT preview_cards_statuses_pkey PRIMARY KEY (status_id, preview_card_id);


--
-- Name: preview_cards preview_cards_url_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards
    ADD CONSTRAINT preview_cards_url_key UNIQUE (url);


--
-- Name: push_delivery_jobs push_delivery_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.push_delivery_jobs
    ADD CONSTRAINT push_delivery_jobs_pkey PRIMARY KEY (id);


--
-- Name: quote_verify_jobs quote_verify_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quote_verify_jobs
    ADD CONSTRAINT quote_verify_jobs_pkey PRIMARY KEY (id);


--
-- Name: quote_verify_jobs quote_verify_jobs_quote_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quote_verify_jobs
    ADD CONSTRAINT quote_verify_jobs_quote_id_key UNIQUE (quote_id);


--
-- Name: quotes quotes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_pkey PRIMARY KEY (id);


--
-- Name: quotes quotes_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_status_id_key UNIQUE (status_id);


--
-- Name: relays relays_inbox_url_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.relays
    ADD CONSTRAINT relays_inbox_url_key UNIQUE (inbox_url);


--
-- Name: relays relays_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.relays
    ADD CONSTRAINT relays_pkey PRIMARY KEY (id);


--
-- Name: remote_fetch_failures remote_fetch_failures_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.remote_fetch_failures
    ADD CONSTRAINT remote_fetch_failures_pkey PRIMARY KEY (scope, failure_key);


--
-- Name: reply_fetch_jobs reply_fetch_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reply_fetch_jobs
    ADD CONSTRAINT reply_fetch_jobs_pkey PRIMARY KEY (id);


--
-- Name: reply_fetch_jobs reply_fetch_jobs_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reply_fetch_jobs
    ADD CONSTRAINT reply_fetch_jobs_status_id_key UNIQUE (status_id);


--
-- Name: report_notes report_notes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_notes
    ADD CONSTRAINT report_notes_pkey PRIMARY KEY (id);


--
-- Name: reports reports_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_pkey PRIMARY KEY (id);


--
-- Name: rule_translations rule_translations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.rule_translations
    ADD CONSTRAINT rule_translations_pkey PRIMARY KEY (id);


--
-- Name: rule_translations rule_translations_rule_id_language_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.rule_translations
    ADD CONSTRAINT rule_translations_rule_id_language_key UNIQUE (rule_id, language);


--
-- Name: rules rules_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.rules
    ADD CONSTRAINT rules_pkey PRIMARY KEY (id);


--
-- Name: scheduled_statuses scheduled_statuses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scheduled_statuses
    ADD CONSTRAINT scheduled_statuses_pkey PRIMARY KEY (id);


--
-- Name: site_upload_variants site_upload_variants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.site_upload_variants
    ADD CONSTRAINT site_upload_variants_pkey PRIMARY KEY (var, style);


--
-- Name: site_uploads site_uploads_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.site_uploads
    ADD CONSTRAINT site_uploads_pkey PRIMARY KEY (var);


--
-- Name: software_updates software_updates_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.software_updates
    ADD CONSTRAINT software_updates_pkey PRIMARY KEY (id);


--
-- Name: software_updates software_updates_version_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.software_updates
    ADD CONSTRAINT software_updates_version_key UNIQUE (version);


--
-- Name: status_conversations status_conversations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_conversations
    ADD CONSTRAINT status_conversations_pkey PRIMARY KEY (status_id);


--
-- Name: status_dislikes status_dislikes_account_id_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_dislikes
    ADD CONSTRAINT status_dislikes_account_id_status_id_key UNIQUE (account_id, status_id);


--
-- Name: status_dislikes status_dislikes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_dislikes
    ADD CONSTRAINT status_dislikes_pkey PRIMARY KEY (id);


--
-- Name: status_edits status_edits_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_edits
    ADD CONSTRAINT status_edits_pkey PRIMARY KEY (id);


--
-- Name: status_events status_events_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_pkey PRIMARY KEY (status_id);


--
-- Name: status_mentions status_mentions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_mentions
    ADD CONSTRAINT status_mentions_pkey PRIMARY KEY (status_id, account_id);


--
-- Name: status_pins status_pins_account_id_status_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_pins
    ADD CONSTRAINT status_pins_account_id_status_id_key UNIQUE (account_id, status_id);


--
-- Name: status_pins status_pins_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_pins
    ADD CONSTRAINT status_pins_pkey PRIMARY KEY (id);


--
-- Name: status_reactions status_reactions_account_id_status_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reactions
    ADD CONSTRAINT status_reactions_account_id_status_id_name_key UNIQUE (account_id, status_id, name);


--
-- Name: status_reactions status_reactions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reactions
    ADD CONSTRAINT status_reactions_pkey PRIMARY KEY (id);


--
-- Name: status_reply_fetches status_reply_fetches_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reply_fetches
    ADD CONSTRAINT status_reply_fetches_pkey PRIMARY KEY (status_id);


--
-- Name: status_tags status_tags_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_tags
    ADD CONSTRAINT status_tags_pkey PRIMARY KEY (status_id, tag_id);


--
-- Name: status_tombstones status_tombstones_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_tombstones
    ADD CONSTRAINT status_tombstones_pkey PRIMARY KEY (status_id);


--
-- Name: status_trends status_trends_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_trends
    ADD CONSTRAINT status_trends_pkey PRIMARY KEY (status_id);


--
-- Name: statuses statuses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.statuses
    ADD CONSTRAINT statuses_pkey PRIMARY KEY (id);


--
-- Name: tag_follows tag_follows_account_id_tag_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_follows
    ADD CONSTRAINT tag_follows_account_id_tag_id_key UNIQUE (account_id, tag_id);


--
-- Name: tag_follows tag_follows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_follows
    ADD CONSTRAINT tag_follows_pkey PRIMARY KEY (id);


--
-- Name: tag_trends tag_trends_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_trends
    ADD CONSTRAINT tag_trends_pkey PRIMARY KEY (tag_id);


--
-- Name: tag_usages tag_usages_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_usages
    ADD CONSTRAINT tag_usages_pkey PRIMARY KEY (tag_id, day, account_id);


--
-- Name: tagged_objects tagged_objects_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tagged_objects
    ADD CONSTRAINT tagged_objects_pkey PRIMARY KEY (id);


--
-- Name: tagged_objects tagged_objects_status_id_collection_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tagged_objects
    ADD CONSTRAINT tagged_objects_status_id_collection_id_key UNIQUE (status_id, collection_id);


--
-- Name: tags tags_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tags
    ADD CONSTRAINT tags_pkey PRIMARY KEY (id);


--
-- Name: terms_of_services terms_of_services_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.terms_of_services
    ADD CONSTRAINT terms_of_services_pkey PRIMARY KEY (id);


--
-- Name: two_factor_challenges two_factor_challenges_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.two_factor_challenges
    ADD CONSTRAINT two_factor_challenges_pkey PRIMARY KEY (id);


--
-- Name: two_factor_challenges two_factor_challenges_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.two_factor_challenges
    ADD CONSTRAINT two_factor_challenges_token_hash_key UNIQUE (token_hash);


--
-- Name: user_roles user_roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_roles
    ADD CONSTRAINT user_roles_pkey PRIMARY KEY (id);


--
-- Name: username_blocks username_blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.username_blocks
    ADD CONSTRAINT username_blocks_pkey PRIMARY KEY (id);


--
-- Name: users users_account_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_account_id_key UNIQUE (account_id);


--
-- Name: users users_confirmation_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_confirmation_token_hash_key UNIQUE (confirmation_token_hash);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: users users_reset_password_token_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_reset_password_token_hash_key UNIQUE (reset_password_token_hash);


--
-- Name: vapid_keys vapid_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.vapid_keys
    ADD CONSTRAINT vapid_keys_pkey PRIMARY KEY (id);


--
-- Name: web_push_alerts web_push_alerts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_alerts
    ADD CONSTRAINT web_push_alerts_pkey PRIMARY KEY (subscription_id, kind);


--
-- Name: web_push_subscriptions web_push_subscriptions_access_token_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_access_token_id_key UNIQUE (access_token_id);


--
-- Name: web_push_subscriptions web_push_subscriptions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_pkey PRIMARY KEY (id);


--
-- Name: web_settings web_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_settings
    ADD CONSTRAINT web_settings_pkey PRIMARY KEY (id);


--
-- Name: web_settings web_settings_user_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_settings
    ADD CONSTRAINT web_settings_user_id_key UNIQUE (user_id);


--
-- Name: webauthn_credentials webauthn_credentials_external_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webauthn_credentials
    ADD CONSTRAINT webauthn_credentials_external_id_key UNIQUE (external_id);


--
-- Name: webauthn_credentials webauthn_credentials_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webauthn_credentials
    ADD CONSTRAINT webauthn_credentials_pkey PRIMARY KEY (id);


--
-- Name: webauthn_credentials webauthn_credentials_user_nickname_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webauthn_credentials
    ADD CONSTRAINT webauthn_credentials_user_nickname_key UNIQUE (user_id, nickname);


--
-- Name: webhook_delivery_jobs webhook_delivery_jobs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webhook_delivery_jobs
    ADD CONSTRAINT webhook_delivery_jobs_pkey PRIMARY KEY (id);


--
-- Name: webhooks webhooks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webhooks
    ADD CONSTRAINT webhooks_pkey PRIMARY KEY (id);


--
-- Name: account_conversations_paging_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_conversations_paging_idx ON public.account_conversations USING btree (account_id, last_status_id DESC);


--
-- Name: account_domain_blocks_domain_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_domain_blocks_domain_idx ON public.account_domain_blocks USING btree (domain);


--
-- Name: account_endorsements_owner_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_endorsements_owner_idx ON public.account_endorsements USING btree (account_id, id DESC);


--
-- Name: account_media_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_media_jobs_run_at_idx ON public.account_media_jobs USING btree (run_at);


--
-- Name: account_migrations_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX account_migrations_account_idx ON public.account_migrations USING btree (account_id, created_at DESC);


--
-- Name: accounts_local_username_unique; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX accounts_local_username_unique ON public.accounts USING btree (lower(username)) WHERE (domain IS NULL);


--
-- Name: admin_action_logs_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX admin_action_logs_account_idx ON public.admin_action_logs USING btree (account_id, id DESC);


--
-- Name: admin_action_logs_target_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX admin_action_logs_target_idx ON public.admin_action_logs USING btree (target_type, target_id, id DESC);


--
-- Name: blocks_target_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX blocks_target_idx ON public.blocks USING btree (target_account_id);


--
-- Name: bookmarks_status_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX bookmarks_status_idx ON public.bookmarks USING btree (status_id);


--
-- Name: conversation_mutes_conversation_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX conversation_mutes_conversation_idx ON public.conversation_mutes USING btree (conversation_id);


--
-- Name: conversations_uri_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX conversations_uri_idx ON public.conversations USING btree (uri) WHERE (uri IS NOT NULL);


--
-- Name: custom_filter_keywords_filter_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX custom_filter_keywords_filter_idx ON public.custom_filter_keywords USING btree (custom_filter_id);


--
-- Name: custom_filter_statuses_filter_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX custom_filter_statuses_filter_idx ON public.custom_filter_statuses USING btree (custom_filter_id);


--
-- Name: custom_filters_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX custom_filters_account_idx ON public.custom_filters USING btree (account_id);


--
-- Name: featured_tags_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX featured_tags_account_idx ON public.featured_tags USING btree (account_id, id DESC);


--
-- Name: idx_account_archives_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_account_archives_account ON public.account_archives USING btree (account_id, id DESC);


--
-- Name: idx_account_archives_scheduled; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_account_archives_scheduled ON public.account_archives USING btree (id) WHERE (state = 'scheduled'::text);


--
-- Name: idx_account_moderation_notes_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_account_moderation_notes_target ON public.account_moderation_notes USING btree (target_account_id);


--
-- Name: idx_account_warnings_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_account_warnings_target ON public.account_warnings USING btree (target_account_id);


--
-- Name: idx_accounts_local; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_accounts_local ON public.accounts USING btree (id) WHERE (domain IS NULL);


--
-- Name: idx_accounts_remote_acct; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_accounts_remote_acct ON public.accounts USING btree (lower(username), lower(domain)) WHERE (domain IS NOT NULL);


--
-- Name: idx_accounts_search; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_accounts_search ON public.accounts USING gin ((((setweight(to_tsvector('simple'::regconfig, display_name), 'A'::"char") || setweight(to_tsvector('simple'::regconfig, username), 'B'::"char")) || setweight(to_tsvector('simple'::regconfig, COALESCE(domain, ''::text)), 'C'::"char"))));


--
-- Name: idx_accounts_uri; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_accounts_uri ON public.accounts USING btree (uri) WHERE (uri IS NOT NULL);


--
-- Name: idx_announcement_reactions_announcement; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_announcement_reactions_announcement ON public.announcement_reactions USING btree (announcement_id);


--
-- Name: idx_announcements_published; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_announcements_published ON public.announcements USING btree (published);


--
-- Name: idx_appeals_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_appeals_account ON public.appeals USING btree (account_id, id DESC);


--
-- Name: idx_appeals_pending; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_appeals_pending ON public.appeals USING btree (id DESC) WHERE ((approved_at IS NULL) AND (rejected_at IS NULL));


--
-- Name: idx_bulk_import_rows_import; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bulk_import_rows_import ON public.bulk_import_rows USING btree (bulk_import_id);


--
-- Name: idx_bulk_imports_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bulk_imports_account ON public.bulk_imports USING btree (account_id, id DESC);


--
-- Name: idx_bulk_imports_scheduled; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_bulk_imports_scheduled ON public.bulk_imports USING btree (id) WHERE (state = 'scheduled'::text);


--
-- Name: idx_canonical_email_blocks_reference; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_canonical_email_blocks_reference ON public.canonical_email_blocks USING btree (reference_account_id) WHERE (reference_account_id IS NOT NULL);


--
-- Name: idx_collection_items_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_collection_items_account ON public.collection_items USING btree (account_id);


--
-- Name: idx_collection_items_collection; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_collection_items_collection ON public.collection_items USING btree (collection_id);


--
-- Name: idx_collection_items_unique_account; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_collection_items_unique_account ON public.collection_items USING btree (collection_id, account_id) WHERE (account_id IS NOT NULL);


--
-- Name: idx_collections_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_collections_account ON public.collections USING btree (account_id);


--
-- Name: idx_delivery_jobs_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_jobs_account ON public.delivery_jobs USING btree (account_id) WHERE (account_id IS NOT NULL);


--
-- Name: idx_delivery_jobs_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_delivery_jobs_due ON public.delivery_jobs USING btree (run_at);


--
-- Name: idx_email_jobs_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_email_jobs_due ON public.email_jobs USING btree (run_at);


--
-- Name: idx_favourites_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_favourites_status ON public.favourites USING btree (status_id);


--
-- Name: idx_follows_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_follows_target ON public.follows USING btree (target_account_id);


--
-- Name: idx_group_affiliations_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_group_affiliations_account ON public.group_affiliations USING btree (account_id);


--
-- Name: idx_invites_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_invites_user ON public.invites USING btree (user_id);


--
-- Name: idx_login_activities_day; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_login_activities_day ON public.login_activities USING btree (created_at, user_id);


--
-- Name: idx_login_activities_user_ip_used; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_login_activities_user_ip_used ON public.login_activities USING btree (user_id, ip, created_at DESC) WHERE (ip IS NOT NULL);


--
-- Name: idx_login_activities_user_recent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_login_activities_user_recent ON public.login_activities USING btree (user_id, created_at DESC);


--
-- Name: idx_media_attachments_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_media_attachments_account ON public.media_attachments USING btree (account_id);


--
-- Name: idx_media_cached_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_media_cached_at ON public.media_attachments USING btree (cached_at) WHERE (cached_at IS NOT NULL);


--
-- Name: idx_media_remote_dedup; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_media_remote_dedup ON public.media_attachments USING btree (status_id, remote_url) WHERE (remote_url IS NOT NULL);


--
-- Name: idx_media_scheduled_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_media_scheduled_status ON public.media_attachments USING btree (scheduled_status_id) WHERE (scheduled_status_id IS NOT NULL);


--
-- Name: idx_media_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_media_status ON public.media_attachments USING btree (status_id) WHERE (status_id IS NOT NULL);


--
-- Name: idx_notification_requests_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notification_requests_account ON public.notification_requests USING btree (account_id, id);


--
-- Name: idx_notifications_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_account ON public.notifications USING btree (account_id, id DESC);


--
-- Name: idx_notifications_collection; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_collection ON public.notifications USING btree (collection_id) WHERE (collection_id IS NOT NULL);


--
-- Name: idx_notifications_filtered; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_filtered ON public.notifications USING btree (account_id, from_account_id) WHERE filtered;


--
-- Name: idx_notifications_from_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_from_account ON public.notifications USING btree (from_account_id);


--
-- Name: idx_notifications_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_notifications_status ON public.notifications USING btree (status_id) WHERE (status_id IS NOT NULL);


--
-- Name: idx_oauth_tokens_user_live; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_oauth_tokens_user_live ON public.oauth_tokens USING btree (user_id, app_id) WHERE ((user_id IS NOT NULL) AND (revoked_at IS NULL));


--
-- Name: idx_poll_votes_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_poll_votes_account ON public.poll_votes USING btree (account_id);


--
-- Name: idx_poll_votes_poll; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_poll_votes_poll ON public.poll_votes USING btree (poll_id);


--
-- Name: idx_poll_votes_uri; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_poll_votes_uri ON public.poll_votes USING btree (uri) WHERE (uri IS NOT NULL);


--
-- Name: idx_polls_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_polls_account ON public.polls USING btree (account_id);


--
-- Name: idx_push_delivery_jobs_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_push_delivery_jobs_due ON public.push_delivery_jobs USING btree (run_at);


--
-- Name: idx_push_jobs_notification; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_push_jobs_notification ON public.push_delivery_jobs USING btree (notification_id);


--
-- Name: idx_push_jobs_subscription; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_push_jobs_subscription ON public.push_delivery_jobs USING btree (subscription_id);


--
-- Name: idx_quotes_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_quotes_account ON public.quotes USING btree (account_id);


--
-- Name: idx_quotes_activity; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_quotes_activity ON public.quotes USING btree (activity_uri) WHERE (activity_uri IS NOT NULL);


--
-- Name: idx_quotes_approval_uri; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_quotes_approval_uri ON public.quotes USING btree (approval_uri) WHERE (approval_uri IS NOT NULL);


--
-- Name: idx_quotes_quoted; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_quotes_quoted ON public.quotes USING btree (quoted_status_id) WHERE (quoted_status_id IS NOT NULL);


--
-- Name: idx_quotes_quoted_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_quotes_quoted_account ON public.quotes USING btree (quoted_account_id) WHERE (quoted_account_id IS NOT NULL);


--
-- Name: idx_quotes_status_uri; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_quotes_status_uri ON public.quotes USING btree (status_uri);


--
-- Name: idx_report_notes_report; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_report_notes_report ON public.report_notes USING btree (report_id);


--
-- Name: idx_reports_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_reports_account ON public.reports USING btree (account_id);


--
-- Name: idx_reports_assigned; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_reports_assigned ON public.reports USING btree (assigned_account_id);


--
-- Name: idx_reports_group; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_reports_group ON public.reports USING btree (group_account_id);


--
-- Name: idx_reports_target; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_reports_target ON public.reports USING btree (target_account_id);


--
-- Name: idx_rules_ordered; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_rules_ordered ON public.rules USING btree (priority, id) WHERE (deleted_at IS NULL);


--
-- Name: idx_scheduled_statuses_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_scheduled_statuses_account ON public.scheduled_statuses USING btree (account_id, id);


--
-- Name: idx_scheduled_statuses_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_scheduled_statuses_due ON public.scheduled_statuses USING btree (scheduled_at);


--
-- Name: idx_software_updates_urgent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_software_updates_urgent ON public.software_updates USING btree (urgent) WHERE urgent;


--
-- Name: idx_status_dislikes_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_dislikes_status ON public.status_dislikes USING btree (status_id);


--
-- Name: idx_status_edits_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_edits_account ON public.status_edits USING btree (account_id);


--
-- Name: idx_status_edits_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_edits_status ON public.status_edits USING btree (status_id, id);


--
-- Name: idx_status_mentions_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_mentions_account ON public.status_mentions USING btree (account_id);


--
-- Name: idx_status_reactions_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_reactions_status ON public.status_reactions USING btree (status_id);


--
-- Name: idx_status_tags_tag; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_tags_tag ON public.status_tags USING btree (tag_id, status_id DESC);


--
-- Name: idx_status_tags_tag_sort_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_tags_tag_sort_at ON public.status_tags USING btree (tag_id, sort_at DESC, status_id DESC);


--
-- Name: idx_status_tombstones_deleted_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_status_tombstones_deleted_at ON public.status_tombstones USING btree (deleted_at);


--
-- Name: idx_statuses_account; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_account ON public.statuses USING btree (account_id, id DESC);


--
-- Name: idx_statuses_account_reblog; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_statuses_account_reblog ON public.statuses USING btree (account_id, reblog_of_id) WHERE (reblog_of_id IS NOT NULL);


--
-- Name: idx_statuses_account_sort_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_account_sort_at ON public.statuses USING btree (account_id, sort_at DESC, id DESC);


--
-- Name: idx_statuses_application; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_application ON public.statuses USING btree (application_id) WHERE (application_id IS NOT NULL);


--
-- Name: idx_statuses_local_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_local_id ON public.statuses USING btree (id) WHERE ((uri IS NULL) AND (reblog_of_id IS NULL) AND (visibility = ANY (ARRAY['public'::text, 'local'::text])));


--
-- Name: idx_statuses_local_sort_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_local_sort_at ON public.statuses USING btree (sort_at, id) WHERE ((uri IS NULL) AND (reblog_of_id IS NULL) AND (visibility = ANY (ARRAY['public'::text, 'local'::text])));


--
-- Name: idx_statuses_public; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_public ON public.statuses USING btree (id DESC) WHERE ((visibility = 'public'::text) AND (reblog_of_id IS NULL));


--
-- Name: idx_statuses_reblog; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_reblog ON public.statuses USING btree (reblog_of_id) WHERE (reblog_of_id IS NOT NULL);


--
-- Name: idx_statuses_reply; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_reply ON public.statuses USING btree (in_reply_to_id) WHERE (in_reply_to_id IS NOT NULL);


--
-- Name: idx_statuses_search; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_search ON public.statuses USING gin (to_tsvector('simple'::regconfig, content)) WHERE (reblog_of_id IS NULL);


--
-- Name: idx_statuses_sort_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_statuses_sort_at ON public.statuses USING btree (sort_at, id);


--
-- Name: idx_statuses_uri; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_statuses_uri ON public.statuses USING btree (uri) WHERE (uri IS NOT NULL);


--
-- Name: idx_tags_name; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_tags_name ON public.tags USING btree (lower(name));


--
-- Name: idx_tags_name_prefix; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_tags_name_prefix ON public.tags USING btree (lower(name) text_pattern_ops);


--
-- Name: idx_terms_of_services_effective_date; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_terms_of_services_effective_date ON public.terms_of_services USING btree (effective_date) WHERE (effective_date IS NOT NULL);


--
-- Name: idx_two_factor_challenges_expires_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_two_factor_challenges_expires_at ON public.two_factor_challenges USING btree (expires_at);


--
-- Name: idx_username_blocks_normalized; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_username_blocks_normalized ON public.username_blocks USING btree (normalized_username);


--
-- Name: idx_username_blocks_username; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_username_blocks_username ON public.username_blocks USING btree (lower(username));


--
-- Name: idx_users_email; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_users_email ON public.users USING btree (lower(email));


--
-- Name: idx_users_role_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_users_role_id ON public.users USING btree (role_id) WHERE (role_id IS NOT NULL);


--
-- Name: idx_web_push_subscriptions_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_web_push_subscriptions_user ON public.web_push_subscriptions USING btree (user_id);


--
-- Name: idx_webauthn_credentials_user_id; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_webauthn_credentials_user_id ON public.webauthn_credentials USING btree (user_id);


--
-- Name: idx_webhook_delivery_jobs_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_webhook_delivery_jobs_due ON public.webhook_delivery_jobs USING btree (run_at);


--
-- Name: idx_webhooks_url; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX idx_webhooks_url ON public.webhooks USING btree (url);


--
-- Name: link_crawl_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX link_crawl_jobs_run_at_idx ON public.link_crawl_jobs USING btree (run_at);


--
-- Name: link_verification_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX link_verification_jobs_run_at_idx ON public.link_verification_jobs USING btree (run_at);


--
-- Name: list_accounts_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX list_accounts_account_idx ON public.list_accounts USING btree (account_id);


--
-- Name: list_accounts_follow_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX list_accounts_follow_idx ON public.list_accounts USING btree (follow_id) WHERE (follow_id IS NOT NULL);


--
-- Name: lists_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX lists_account_idx ON public.lists USING btree (account_id);


--
-- Name: media_hls_segments_media_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX media_hls_segments_media_id_idx ON public.media_hls_segments USING btree (media_id);


--
-- Name: media_processing_jobs_media_id_key; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX media_processing_jobs_media_id_key ON public.media_processing_jobs USING btree (media_id);


--
-- Name: media_processing_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX media_processing_jobs_run_at_idx ON public.media_processing_jobs USING btree (run_at);


--
-- Name: media_renditions_media_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX media_renditions_media_id_idx ON public.media_renditions USING btree (media_id);


--
-- Name: mutes_target_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX mutes_target_idx ON public.mutes USING btree (target_account_id);


--
-- Name: notifications_account_group_key_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX notifications_account_group_key_idx ON public.notifications USING btree (account_id, group_key) WHERE (group_key IS NOT NULL);


--
-- Name: polls_due_expiry_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX polls_due_expiry_idx ON public.polls USING btree (expires_at) WHERE ((expires_at IS NOT NULL) AND (expiry_processed_at IS NULL));


--
-- Name: preview_card_trends_allowed_score_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX preview_card_trends_allowed_score_idx ON public.preview_card_trends USING btree (allowed, score DESC);


--
-- Name: preview_cards_statuses_card_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX preview_cards_statuses_card_idx ON public.preview_cards_statuses USING btree (preview_card_id);


--
-- Name: quote_verify_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX quote_verify_jobs_run_at_idx ON public.quote_verify_jobs USING btree (run_at);


--
-- Name: reply_fetch_jobs_run_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX reply_fetch_jobs_run_at_idx ON public.reply_fetch_jobs USING btree (run_at);


--
-- Name: status_conversations_conversation_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX status_conversations_conversation_idx ON public.status_conversations USING btree (conversation_id);


--
-- Name: status_pins_status_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX status_pins_status_idx ON public.status_pins USING btree (status_id);


--
-- Name: status_trends_account_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX status_trends_account_idx ON public.status_trends USING btree (account_id);


--
-- Name: status_trends_allowed_score_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX status_trends_allowed_score_idx ON public.status_trends USING btree (allowed, score DESC);


--
-- Name: tag_follows_tag_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tag_follows_tag_idx ON public.tag_follows USING btree (tag_id);


--
-- Name: tag_trends_allowed_score_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tag_trends_allowed_score_idx ON public.tag_trends USING btree (allowed, score DESC);


--
-- Name: tagged_objects_collection_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tagged_objects_collection_id_idx ON public.tagged_objects USING btree (collection_id);


--
-- Name: account_conversations account_conversations_streaming_insert; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER account_conversations_streaming_insert AFTER INSERT ON public.account_conversations FOR EACH ROW EXECUTE FUNCTION public.streaming_conversation_event();


--
-- Name: account_conversations account_conversations_streaming_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER account_conversations_streaming_update AFTER UPDATE ON public.account_conversations FOR EACH ROW WHEN (((new.last_status_id IS NOT NULL) AND (new.last_status_id IS DISTINCT FROM old.last_status_id) AND ((old.last_status_id IS NULL) OR (new.last_status_id > old.last_status_id)))) EXECUTE FUNCTION public.streaming_conversation_event();


--
-- Name: notifications notifications_streaming_event; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER notifications_streaming_event AFTER INSERT ON public.notifications FOR EACH ROW EXECUTE FUNCTION public.streaming_notification_event();


--
-- Name: notifications notifications_web_push_fanout; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER notifications_web_push_fanout AFTER INSERT ON public.notifications FOR EACH ROW EXECUTE FUNCTION public.web_push_fanout();


--
-- Name: account_aliases account_aliases_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_aliases
    ADD CONSTRAINT account_aliases_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_archives account_archives_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_archives
    ADD CONSTRAINT account_archives_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_conversations account_conversations_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_conversations
    ADD CONSTRAINT account_conversations_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_conversations account_conversations_conversation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_conversations
    ADD CONSTRAINT account_conversations_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES public.conversations(id) ON DELETE CASCADE;


--
-- Name: account_domain_blocks account_domain_blocks_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_domain_blocks
    ADD CONSTRAINT account_domain_blocks_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_endorsements account_endorsements_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_endorsements
    ADD CONSTRAINT account_endorsements_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_endorsements account_endorsements_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_endorsements
    ADD CONSTRAINT account_endorsements_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_fields account_fields_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_fields
    ADD CONSTRAINT account_fields_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_media_jobs account_media_jobs_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_media_jobs
    ADD CONSTRAINT account_media_jobs_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_migrations account_migrations_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_migrations
    ADD CONSTRAINT account_migrations_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_migrations account_migrations_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_migrations
    ADD CONSTRAINT account_migrations_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: account_moderation_notes account_moderation_notes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_moderation_notes
    ADD CONSTRAINT account_moderation_notes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: account_moderation_notes account_moderation_notes_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_moderation_notes
    ADD CONSTRAINT account_moderation_notes_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_notes account_notes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_notes
    ADD CONSTRAINT account_notes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_notes account_notes_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_notes
    ADD CONSTRAINT account_notes_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_statuses_cleanup_policies account_statuses_cleanup_policies_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_statuses_cleanup_policies
    ADD CONSTRAINT account_statuses_cleanup_policies_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: account_warnings account_warnings_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_warnings
    ADD CONSTRAINT account_warnings_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: account_warnings account_warnings_report_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_warnings
    ADD CONSTRAINT account_warnings_report_id_fkey FOREIGN KEY (report_id) REFERENCES public.reports(id) ON DELETE SET NULL;


--
-- Name: account_warnings account_warnings_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.account_warnings
    ADD CONSTRAINT account_warnings_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: admin_action_logs admin_action_logs_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.admin_action_logs
    ADD CONSTRAINT admin_action_logs_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: announcement_mutes announcement_mutes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_mutes
    ADD CONSTRAINT announcement_mutes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: announcement_mutes announcement_mutes_announcement_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_mutes
    ADD CONSTRAINT announcement_mutes_announcement_id_fkey FOREIGN KEY (announcement_id) REFERENCES public.announcements(id) ON DELETE CASCADE;


--
-- Name: announcement_reactions announcement_reactions_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_reactions
    ADD CONSTRAINT announcement_reactions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: announcement_reactions announcement_reactions_announcement_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_reactions
    ADD CONSTRAINT announcement_reactions_announcement_id_fkey FOREIGN KEY (announcement_id) REFERENCES public.announcements(id) ON DELETE CASCADE;


--
-- Name: announcement_reactions announcement_reactions_custom_emoji_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.announcement_reactions
    ADD CONSTRAINT announcement_reactions_custom_emoji_id_fkey FOREIGN KEY (custom_emoji_id) REFERENCES public.custom_emojis(id) ON DELETE CASCADE;


--
-- Name: appeals appeals_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: appeals appeals_account_warning_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_account_warning_id_fkey FOREIGN KEY (account_warning_id) REFERENCES public.account_warnings(id) ON DELETE CASCADE;


--
-- Name: appeals appeals_approved_by_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_approved_by_account_id_fkey FOREIGN KEY (approved_by_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: appeals appeals_rejected_by_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.appeals
    ADD CONSTRAINT appeals_rejected_by_account_id_fkey FOREIGN KEY (rejected_by_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: blocks blocks_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.blocks
    ADD CONSTRAINT blocks_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: blocks blocks_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.blocks
    ADD CONSTRAINT blocks_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: bookmarks bookmarks_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bookmarks
    ADD CONSTRAINT bookmarks_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: bookmarks bookmarks_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bookmarks
    ADD CONSTRAINT bookmarks_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: bulk_import_row_languages bulk_import_row_languages_bulk_import_row_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_import_row_languages
    ADD CONSTRAINT bulk_import_row_languages_bulk_import_row_id_fkey FOREIGN KEY (bulk_import_row_id) REFERENCES public.bulk_import_rows(id) ON DELETE CASCADE;


--
-- Name: bulk_import_rows bulk_import_rows_bulk_import_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_import_rows
    ADD CONSTRAINT bulk_import_rows_bulk_import_id_fkey FOREIGN KEY (bulk_import_id) REFERENCES public.bulk_imports(id) ON DELETE CASCADE;


--
-- Name: bulk_imports bulk_imports_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.bulk_imports
    ADD CONSTRAINT bulk_imports_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: canonical_email_blocks canonical_email_blocks_reference_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.canonical_email_blocks
    ADD CONSTRAINT canonical_email_blocks_reference_account_id_fkey FOREIGN KEY (reference_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: collection_items collection_items_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collection_items
    ADD CONSTRAINT collection_items_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: collection_items collection_items_collection_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collection_items
    ADD CONSTRAINT collection_items_collection_id_fkey FOREIGN KEY (collection_id) REFERENCES public.collections(id) ON DELETE CASCADE;


--
-- Name: collections collections_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: collections collections_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE SET NULL;


--
-- Name: conversation_mutes conversation_mutes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_mutes
    ADD CONSTRAINT conversation_mutes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: conversation_mutes conversation_mutes_conversation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_mutes
    ADD CONSTRAINT conversation_mutes_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES public.conversations(id) ON DELETE CASCADE;


--
-- Name: conversations conversations_owner_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversations
    ADD CONSTRAINT conversations_owner_account_id_fkey FOREIGN KEY (owner_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: conversations conversations_root_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversations
    ADD CONSTRAINT conversations_root_status_id_fkey FOREIGN KEY (root_status_id) REFERENCES public.statuses(id) ON DELETE SET NULL;


--
-- Name: custom_filter_keywords custom_filter_keywords_custom_filter_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_keywords
    ADD CONSTRAINT custom_filter_keywords_custom_filter_id_fkey FOREIGN KEY (custom_filter_id) REFERENCES public.custom_filters(id) ON DELETE CASCADE;


--
-- Name: custom_filter_statuses custom_filter_statuses_custom_filter_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_statuses
    ADD CONSTRAINT custom_filter_statuses_custom_filter_id_fkey FOREIGN KEY (custom_filter_id) REFERENCES public.custom_filters(id) ON DELETE CASCADE;


--
-- Name: custom_filter_statuses custom_filter_statuses_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filter_statuses
    ADD CONSTRAINT custom_filter_statuses_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: custom_filters custom_filters_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.custom_filters
    ADD CONSTRAINT custom_filters_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: delivery_jobs delivery_jobs_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.delivery_jobs
    ADD CONSTRAINT delivery_jobs_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: email_domain_blocks email_domain_blocks_parent_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.email_domain_blocks
    ADD CONSTRAINT email_domain_blocks_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.email_domain_blocks(id) ON DELETE CASCADE;


--
-- Name: favourites favourites_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favourites
    ADD CONSTRAINT favourites_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: favourites favourites_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favourites
    ADD CONSTRAINT favourites_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: featured_tags featured_tags_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.featured_tags
    ADD CONSTRAINT featured_tags_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: featured_tags featured_tags_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.featured_tags
    ADD CONSTRAINT featured_tags_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE CASCADE;


--
-- Name: follow_recommendation_mutes follow_recommendation_mutes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follow_recommendation_mutes
    ADD CONSTRAINT follow_recommendation_mutes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: follow_recommendation_mutes follow_recommendation_mutes_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follow_recommendation_mutes
    ADD CONSTRAINT follow_recommendation_mutes_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: follows follows_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follows
    ADD CONSTRAINT follows_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: follows follows_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.follows
    ADD CONSTRAINT follows_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: group_affiliations group_affiliations_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_affiliations
    ADD CONSTRAINT group_affiliations_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: group_affiliations group_affiliations_group_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_affiliations
    ADD CONSTRAINT group_affiliations_group_account_id_fkey FOREIGN KEY (group_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: group_locked_posts group_locked_posts_group_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_locked_posts
    ADD CONSTRAINT group_locked_posts_group_account_id_fkey FOREIGN KEY (group_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: group_locked_posts group_locked_posts_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.group_locked_posts
    ADD CONSTRAINT group_locked_posts_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: groups groups_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.groups
    ADD CONSTRAINT groups_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: groups groups_created_by_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.groups
    ADD CONSTRAINT groups_created_by_fkey FOREIGN KEY (created_by) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: invites invites_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.invites
    ADD CONSTRAINT invites_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: link_crawl_jobs link_crawl_jobs_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.link_crawl_jobs
    ADD CONSTRAINT link_crawl_jobs_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: link_verification_jobs link_verification_jobs_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.link_verification_jobs
    ADD CONSTRAINT link_verification_jobs_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: list_accounts list_accounts_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.list_accounts
    ADD CONSTRAINT list_accounts_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: list_accounts list_accounts_follow_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.list_accounts
    ADD CONSTRAINT list_accounts_follow_id_fkey FOREIGN KEY (follow_id) REFERENCES public.follows(id) ON DELETE CASCADE;


--
-- Name: list_accounts list_accounts_list_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.list_accounts
    ADD CONSTRAINT list_accounts_list_id_fkey FOREIGN KEY (list_id) REFERENCES public.lists(id) ON DELETE CASCADE;


--
-- Name: lists lists_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.lists
    ADD CONSTRAINT lists_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: login_activities login_activities_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.login_activities
    ADD CONSTRAINT login_activities_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: markers markers_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.markers
    ADD CONSTRAINT markers_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: media_attachments media_attachments_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_attachments
    ADD CONSTRAINT media_attachments_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: media_attachments media_attachments_scheduled_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_attachments
    ADD CONSTRAINT media_attachments_scheduled_status_id_fkey FOREIGN KEY (scheduled_status_id) REFERENCES public.scheduled_statuses(id) ON DELETE SET NULL;


--
-- Name: media_attachments media_attachments_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_attachments
    ADD CONSTRAINT media_attachments_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: media_hls_segments media_hls_segments_media_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_hls_segments
    ADD CONSTRAINT media_hls_segments_media_id_fkey FOREIGN KEY (media_id) REFERENCES public.media_attachments(id) ON DELETE CASCADE;


--
-- Name: media_processing_jobs media_processing_jobs_media_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_processing_jobs
    ADD CONSTRAINT media_processing_jobs_media_id_fkey FOREIGN KEY (media_id) REFERENCES public.media_attachments(id) ON DELETE CASCADE;


--
-- Name: media_renditions media_renditions_media_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.media_renditions
    ADD CONSTRAINT media_renditions_media_id_fkey FOREIGN KEY (media_id) REFERENCES public.media_attachments(id) ON DELETE CASCADE;


--
-- Name: mutes mutes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mutes
    ADD CONSTRAINT mutes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: mutes mutes_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.mutes
    ADD CONSTRAINT mutes_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_permissions notification_permissions_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_permissions
    ADD CONSTRAINT notification_permissions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_permissions notification_permissions_from_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_permissions
    ADD CONSTRAINT notification_permissions_from_account_id_fkey FOREIGN KEY (from_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_policies notification_policies_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_policies
    ADD CONSTRAINT notification_policies_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_requests notification_requests_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_requests
    ADD CONSTRAINT notification_requests_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_requests notification_requests_from_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_requests
    ADD CONSTRAINT notification_requests_from_account_id_fkey FOREIGN KEY (from_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notification_requests notification_requests_last_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notification_requests
    ADD CONSTRAINT notification_requests_last_status_id_fkey FOREIGN KEY (last_status_id) REFERENCES public.statuses(id) ON DELETE SET NULL;


--
-- Name: notifications notifications_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notifications notifications_collection_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_collection_id_fkey FOREIGN KEY (collection_id) REFERENCES public.collections(id) ON DELETE CASCADE;


--
-- Name: notifications notifications_from_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_from_account_id_fkey FOREIGN KEY (from_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: notifications notifications_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.notifications
    ADD CONSTRAINT notifications_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: oauth_grants oauth_grants_app_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_grants
    ADD CONSTRAINT oauth_grants_app_id_fkey FOREIGN KEY (app_id) REFERENCES public.oauth_apps(id) ON DELETE CASCADE;


--
-- Name: oauth_grants oauth_grants_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_grants
    ADD CONSTRAINT oauth_grants_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: oauth_tokens oauth_tokens_app_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT oauth_tokens_app_id_fkey FOREIGN KEY (app_id) REFERENCES public.oauth_apps(id) ON DELETE CASCADE;


--
-- Name: oauth_tokens oauth_tokens_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_tokens
    ADD CONSTRAINT oauth_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: otp_backup_codes otp_backup_codes_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.otp_backup_codes
    ADD CONSTRAINT otp_backup_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: poll_votes poll_votes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.poll_votes
    ADD CONSTRAINT poll_votes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: poll_votes poll_votes_poll_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.poll_votes
    ADD CONSTRAINT poll_votes_poll_id_fkey FOREIGN KEY (poll_id) REFERENCES public.polls(id) ON DELETE CASCADE;


--
-- Name: polls polls_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.polls
    ADD CONSTRAINT polls_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: polls polls_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.polls
    ADD CONSTRAINT polls_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: preview_card_trends preview_card_trends_preview_card_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_trends
    ADD CONSTRAINT preview_card_trends_preview_card_id_fkey FOREIGN KEY (preview_card_id) REFERENCES public.preview_cards(id) ON DELETE CASCADE;


--
-- Name: preview_card_usages preview_card_usages_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_usages
    ADD CONSTRAINT preview_card_usages_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: preview_card_usages preview_card_usages_preview_card_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_card_usages
    ADD CONSTRAINT preview_card_usages_preview_card_id_fkey FOREIGN KEY (preview_card_id) REFERENCES public.preview_cards(id) ON DELETE CASCADE;


--
-- Name: preview_cards preview_cards_author_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards
    ADD CONSTRAINT preview_cards_author_account_id_fkey FOREIGN KEY (author_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: preview_cards_statuses preview_cards_statuses_preview_card_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards_statuses
    ADD CONSTRAINT preview_cards_statuses_preview_card_id_fkey FOREIGN KEY (preview_card_id) REFERENCES public.preview_cards(id) ON DELETE CASCADE;


--
-- Name: preview_cards_statuses preview_cards_statuses_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.preview_cards_statuses
    ADD CONSTRAINT preview_cards_statuses_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: push_delivery_jobs push_delivery_jobs_notification_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.push_delivery_jobs
    ADD CONSTRAINT push_delivery_jobs_notification_id_fkey FOREIGN KEY (notification_id) REFERENCES public.notifications(id) ON DELETE CASCADE;


--
-- Name: push_delivery_jobs push_delivery_jobs_subscription_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.push_delivery_jobs
    ADD CONSTRAINT push_delivery_jobs_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES public.web_push_subscriptions(id) ON DELETE CASCADE;


--
-- Name: quote_verify_jobs quote_verify_jobs_quote_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quote_verify_jobs
    ADD CONSTRAINT quote_verify_jobs_quote_id_fkey FOREIGN KEY (quote_id) REFERENCES public.quotes(id) ON DELETE CASCADE;


--
-- Name: quotes quotes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: quotes quotes_quoted_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_quoted_account_id_fkey FOREIGN KEY (quoted_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: quotes quotes_quoted_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_quoted_status_id_fkey FOREIGN KEY (quoted_status_id) REFERENCES public.statuses(id) ON DELETE SET NULL;


--
-- Name: quotes quotes_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quotes
    ADD CONSTRAINT quotes_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: reply_fetch_jobs reply_fetch_jobs_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reply_fetch_jobs
    ADD CONSTRAINT reply_fetch_jobs_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: report_notes report_notes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_notes
    ADD CONSTRAINT report_notes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: report_notes report_notes_report_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.report_notes
    ADD CONSTRAINT report_notes_report_id_fkey FOREIGN KEY (report_id) REFERENCES public.reports(id) ON DELETE CASCADE;


--
-- Name: reports reports_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: reports reports_action_taken_by_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_action_taken_by_account_id_fkey FOREIGN KEY (action_taken_by_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: reports reports_assigned_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_assigned_account_id_fkey FOREIGN KEY (assigned_account_id) REFERENCES public.accounts(id) ON DELETE SET NULL;


--
-- Name: reports reports_group_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_group_account_id_fkey FOREIGN KEY (group_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: reports reports_target_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.reports
    ADD CONSTRAINT reports_target_account_id_fkey FOREIGN KEY (target_account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: rule_translations rule_translations_rule_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.rule_translations
    ADD CONSTRAINT rule_translations_rule_id_fkey FOREIGN KEY (rule_id) REFERENCES public.rules(id) ON DELETE CASCADE;


--
-- Name: scheduled_statuses scheduled_statuses_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scheduled_statuses
    ADD CONSTRAINT scheduled_statuses_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: scheduled_statuses scheduled_statuses_application_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scheduled_statuses
    ADD CONSTRAINT scheduled_statuses_application_id_fkey FOREIGN KEY (application_id) REFERENCES public.oauth_apps(id) ON DELETE SET NULL;


--
-- Name: site_upload_variants site_upload_variants_var_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.site_upload_variants
    ADD CONSTRAINT site_upload_variants_var_fkey FOREIGN KEY (var) REFERENCES public.site_uploads(var) ON DELETE CASCADE;


--
-- Name: status_conversations status_conversations_conversation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_conversations
    ADD CONSTRAINT status_conversations_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES public.conversations(id) ON DELETE CASCADE;


--
-- Name: status_conversations status_conversations_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_conversations
    ADD CONSTRAINT status_conversations_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_dislikes status_dislikes_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_dislikes
    ADD CONSTRAINT status_dislikes_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_dislikes status_dislikes_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_dislikes
    ADD CONSTRAINT status_dislikes_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_edits status_edits_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_edits
    ADD CONSTRAINT status_edits_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_edits status_edits_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_edits
    ADD CONSTRAINT status_edits_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_events status_events_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_events
    ADD CONSTRAINT status_events_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_mentions status_mentions_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_mentions
    ADD CONSTRAINT status_mentions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_mentions status_mentions_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_mentions
    ADD CONSTRAINT status_mentions_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_pins status_pins_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_pins
    ADD CONSTRAINT status_pins_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_pins status_pins_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_pins
    ADD CONSTRAINT status_pins_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_reactions status_reactions_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reactions
    ADD CONSTRAINT status_reactions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_reactions status_reactions_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reactions
    ADD CONSTRAINT status_reactions_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_reply_fetches status_reply_fetches_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_reply_fetches
    ADD CONSTRAINT status_reply_fetches_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_tags status_tags_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_tags
    ADD CONSTRAINT status_tags_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: status_tags status_tags_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_tags
    ADD CONSTRAINT status_tags_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE CASCADE;


--
-- Name: status_tombstones status_tombstones_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_tombstones
    ADD CONSTRAINT status_tombstones_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_trends status_trends_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_trends
    ADD CONSTRAINT status_trends_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: status_trends status_trends_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.status_trends
    ADD CONSTRAINT status_trends_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: statuses statuses_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.statuses
    ADD CONSTRAINT statuses_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: statuses statuses_application_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.statuses
    ADD CONSTRAINT statuses_application_id_fkey FOREIGN KEY (application_id) REFERENCES public.oauth_apps(id) ON DELETE SET NULL;


--
-- Name: statuses statuses_in_reply_to_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.statuses
    ADD CONSTRAINT statuses_in_reply_to_id_fkey FOREIGN KEY (in_reply_to_id) REFERENCES public.statuses(id) ON DELETE SET NULL;


--
-- Name: statuses statuses_reblog_of_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.statuses
    ADD CONSTRAINT statuses_reblog_of_id_fkey FOREIGN KEY (reblog_of_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: tag_follows tag_follows_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_follows
    ADD CONSTRAINT tag_follows_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: tag_follows tag_follows_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_follows
    ADD CONSTRAINT tag_follows_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE CASCADE;


--
-- Name: tag_trends tag_trends_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_trends
    ADD CONSTRAINT tag_trends_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE CASCADE;


--
-- Name: tag_usages tag_usages_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_usages
    ADD CONSTRAINT tag_usages_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: tag_usages tag_usages_tag_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tag_usages
    ADD CONSTRAINT tag_usages_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES public.tags(id) ON DELETE CASCADE;


--
-- Name: tagged_objects tagged_objects_collection_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tagged_objects
    ADD CONSTRAINT tagged_objects_collection_id_fkey FOREIGN KEY (collection_id) REFERENCES public.collections(id) ON DELETE CASCADE;


--
-- Name: tagged_objects tagged_objects_status_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tagged_objects
    ADD CONSTRAINT tagged_objects_status_id_fkey FOREIGN KEY (status_id) REFERENCES public.statuses(id) ON DELETE CASCADE;


--
-- Name: two_factor_challenges two_factor_challenges_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.two_factor_challenges
    ADD CONSTRAINT two_factor_challenges_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: users users_account_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;


--
-- Name: users users_created_by_application_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_created_by_application_id_fkey FOREIGN KEY (created_by_application_id) REFERENCES public.oauth_apps(id) ON DELETE SET NULL;


--
-- Name: users users_invite_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_invite_id_fkey FOREIGN KEY (invite_id) REFERENCES public.invites(id) ON DELETE SET NULL;


--
-- Name: users users_role_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_role_id_fkey FOREIGN KEY (role_id) REFERENCES public.user_roles(id) ON DELETE SET NULL;


--
-- Name: web_push_alerts web_push_alerts_subscription_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_alerts
    ADD CONSTRAINT web_push_alerts_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES public.web_push_subscriptions(id) ON DELETE CASCADE;


--
-- Name: web_push_subscriptions web_push_subscriptions_access_token_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_access_token_id_fkey FOREIGN KEY (access_token_id) REFERENCES public.oauth_tokens(id) ON DELETE CASCADE;


--
-- Name: web_push_subscriptions web_push_subscriptions_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_push_subscriptions
    ADD CONSTRAINT web_push_subscriptions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: web_settings web_settings_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.web_settings
    ADD CONSTRAINT web_settings_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: webauthn_credentials webauthn_credentials_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webauthn_credentials
    ADD CONSTRAINT webauthn_credentials_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: webhook_delivery_jobs webhook_delivery_jobs_webhook_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webhook_delivery_jobs
    ADD CONSTRAINT webhook_delivery_jobs_webhook_id_fkey FOREIGN KEY (webhook_id) REFERENCES public.webhooks(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--



-- Required singleton configuration and built-in roles.
INSERT INTO public.instance_settings DEFAULT VALUES;

INSERT INTO public.user_roles (id, name, color, "position", permissions, highlighted) VALUES
    (1, 'Moderator', '#79bd9a', 10, 1944, TRUE),
    (2, 'Admin', '#f4900c', 50, 1048574, TRUE),
    (3, 'Owner', '#ff5050', 100, 1048575, TRUE);
