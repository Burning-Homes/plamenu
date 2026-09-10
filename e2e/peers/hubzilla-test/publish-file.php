<?php

// Turn a WebDAV-uploaded file into Hubzilla's native public file activity.
// This is deliberately a thin wrapper over the upstream attach_store_item()
// path, which selects Image, Audio, Video, or Document from the MIME family.
require_once 'include/cli_startup.php';
cli_startup();
require_once 'include/attach.php';
require_once 'include/channel.php';

if ($argc !== 3) {
    fwrite(STDERR, "usage: publish-file.php CHANNEL FILE-NAME\n");
    exit(2);
}

$channel = channelx_by_nick($argv[1]);
if (!$channel) {
    fwrite(STDERR, "unknown channel: {$argv[1]}\n");
    exit(1);
}
$file = q(
    "SELECT * FROM attach WHERE uid = %d AND filename = '%s' AND is_dir = 0"
    . " ORDER BY edited DESC LIMIT 1",
    intval($channel['channel_id']),
    dbesc($argv[2])
);
if (!$file) {
    fwrite(STDERR, "uploaded file not found: {$argv[2]}\n");
    exit(1);
}

App::set_channel($channel);
App::set_observer($channel);
attach_store_item($channel, $channel, $file[0]);

$item = q(
    "SELECT mid, obj_type FROM item WHERE uid = %d AND resource_type = 'attach'"
    . " AND resource_id = '%s' ORDER BY id DESC LIMIT 1",
    intval($channel['channel_id']),
    dbesc($file[0]['hash'])
);
if (!$item) {
    fwrite(STDERR, "Hubzilla did not create a file activity\n");
    exit(1);
}
echo $item[0]['obj_type'] . ' ' . $item[0]['mid'] . "\n";
