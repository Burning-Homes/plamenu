#!/bin/sh
set -eu

mkdir -p /var/www/html/store/'[data]'
chown -R www-data:www-data /var/www/html/store

exec docker-php-entrypoint apache2-foreground
