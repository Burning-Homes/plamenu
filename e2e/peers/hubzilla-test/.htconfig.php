<?php

// Disposable, source-backed Hubzilla 11.4 fixture configuration.
$db_host = 'hubzilla-db';
$db_port = 3306;
$db_user = 'hubzilla';
$db_pass = 'hubzilla';
$db_data = 'hubzilla';
$db_type = 0;

App::$config['system']['db_skip_locked_supported'] = 1;
App::$config['system']['timezone'] = 'UTC';
App::$config['system']['baseurl'] = 'https://hubzilla.local';
App::$config['system']['sitename'] = 'Hubzilla interop peer';
App::$config['system']['location_hash'] = 'hubzilla-local-e2e-11-4';
App::$config['system']['transport_security_header'] = 1;
App::$config['system']['content_security_policy'] = 1;
App::$config['system']['ssl_cookie_protection'] = 1;
App::$config['system']['register_policy'] = REGISTER_CLOSED;
App::$config['system']['verify_email'] = 0;
App::$config['system']['access_policy'] = ACCESS_FREE;
App::$config['system']['php_path'] = 'php';
App::$config['system']['directory_mode'] = DIRECTORY_MODE_STANDALONE;
App::$config['system']['theme'] = 'redbasic';
App::$config['system']['admin_email'] = 'hazel@hubzilla.local';
App::$config['system']['maxfilesize'] = 134217728;
