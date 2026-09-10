<?php

// Idempotently create the standing Hubzilla account and its public channel.
// Run from /var/www/html after PubCrawl has been globally enabled.
require_once 'include/cli_startup.php';
cli_startup();
require_once 'include/account.php';
require_once 'include/channel.php';

use Zotlabs\Lib\Apps;
use Zotlabs\Access\PermissionRoles;
use Zotlabs\Access\Permissions;

$email = 'hazel@hubzilla.local';
$password = 'hubzilla-hazel-pass-123';
$nickname = 'hazel';

$account = q(
    "SELECT * FROM account WHERE account_email = '%s' LIMIT 1",
    dbesc($email)
);

if (!$account) {
    $salt = random_string(32);
    $encoded = hash('whirlpool', $salt . $password);
    $now = datetime_convert();
    $expires = DBA::$dba->get_null_date();
    $ok = q(
        "INSERT INTO account (account_parent, account_salt, account_password, account_email,"
        . " account_language, account_created, account_flags, account_roles, account_level,"
        . " account_expires, account_service_class) VALUES"
        . " (0, '%s', '%s', '%s', 'en', '%s', 0, %d, 5, '%s', '')",
        dbesc($salt),
        dbesc($encoded),
        dbesc($email),
        dbesc($now),
        intval(ACCOUNT_ROLE_ADMIN),
        dbesc($expires)
    );
    if (!$ok) {
        fwrite(STDERR, "failed to insert Hubzilla account\n");
        exit(1);
    }
    $account = q(
        "SELECT * FROM account WHERE account_email = '%s' LIMIT 1",
        dbesc($email)
    );
    q(
        "UPDATE account SET account_parent = %d WHERE account_id = %d",
        intval($account[0]['account_id']),
        intval($account[0]['account_id'])
    );
}

$channel = channelx_by_nick($nickname);
if (!$channel) {
    $created = create_identity([
        'account_id' => intval($account[0]['account_id']),
        'nickname' => $nickname,
        'name' => 'Hazel',
        'permissions_role' => 'public',
        'publish' => 1,
        'primary' => 1,
    ]);
    if (empty($created['success'])) {
        fwrite(STDERR, 'failed to create Hubzilla channel: ' . ($created['message'] ?? 'unknown error') . "\n");
        exit(1);
    }
    $channel = channelx_by_nick($nickname);
}

$channel_store = 'store/' . $nickname;
if (!is_dir($channel_store)) {
    mkdir($channel_store, 0770, true);
}
chown($channel_store, 'www-data');
chgrp($channel_store, 'www-data');
chmod($channel_store, 0770);

q(
    "UPDATE account SET account_default_channel = %d WHERE account_id = %d",
    intval($channel['channel_id']),
    intval($account[0]['account_id'])
);

// The normal first browser session imports portable app descriptions. A
// headless fixture has no browser session, so import PubCrawl's own descriptor
// with the same Apps APIs (sync=true only bypasses the local-session gate).
App::set_account($account[0]);
App::set_channel($channel);
App::set_observer($channel);
$pubcrawl = Apps::parse_app_description(
    'addon/pubcrawl/pubcrawl.apd',
    false,
    true
);
$pubcrawl['uid'] = 0;
$pubcrawl['guid'] = hash('whirlpool', $pubcrawl['name']);
$pubcrawl['system'] = 1;
$pubcrawl['plugin'] = 'pubcrawl';
Apps::app_install(0, $pubcrawl);
Apps::app_install(intval($channel['channel_id']), 'Activitypub Protocol');
if (!Apps::addon_app_installed(intval($channel['channel_id']), 'pubcrawl')) {
    fwrite(STDERR, "failed to install Activitypub Protocol for Hazel\n");
    exit(1);
}

// The fixture must be followable without a browser approval step so live E2E
// tests can exercise PubCrawl's Create delivery, not only object resolution.
// Hubzilla's `public` role is publicly readable but does not itself enable
// automatic connection approval.
$public = PermissionRoles::role_perms('public');
$autoperms = Permissions::FilledPerms($public['perms_connect']);
set_pconfig(intval($channel['channel_id']), 'system', 'autoperms', 1);
foreach ($autoperms as $permission => $value) {
    set_pconfig(
        intval($channel['channel_id']),
        'autoperms',
        $permission,
        intval($value)
    );
}

echo 'ready: @' . $nickname . '@hubzilla.local (channel ' . $channel['channel_id'] . ")\n";
