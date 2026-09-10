FROM php:8.4-apache-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl \
      libcurl4-openssl-dev libfreetype6-dev libgmp-dev libicu-dev \
      libjpeg62-turbo-dev libonig-dev libpng-dev libxml2-dev libzip-dev \
 && docker-php-ext-configure gd --with-freetype --with-jpeg \
 && docker-php-ext-install -j"$(nproc)" \
      curl dom gd gmp intl mbstring mysqli pdo_mysql simplexml xml xmlreader xmlwriter zip \
 && a2enmod expires headers rewrite \
 && sed -ri 's!AllowOverride None!AllowOverride All!g' /etc/apache2/apache2.conf \
 && rm -rf /var/lib/apt/lists/*

RUN printf '%s\n' \
      'memory_limit=256M' \
      'upload_max_filesize=128M' \
      'post_max_size=128M' \
      'max_execution_time=120' \
      'date.timezone=UTC' \
      > /usr/local/etc/php/conf.d/hubzilla-test.ini

WORKDIR /var/www/html
