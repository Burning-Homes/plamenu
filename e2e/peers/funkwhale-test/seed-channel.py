"""Idempotently create the standing followable Funkwhale audio channel."""

from funkwhale_api.audio import models as audio_models
from funkwhale_api.audio import serializers as audio_serializers
from funkwhale_api.common import preferences
from funkwhale_api.users import models as user_models

# Exercise Funkwhale's default-private media path. A conforming remote server
# retrieves the listen URL with its ActivityPub actor signature rather than a
# Funkwhale user token.
preferences.set("common__api_authentication_required", True)

user = user_models.User.objects.select_related("actor").get(username="fiona")
existing = audio_models.Channel.objects.filter(
    actor__preferred_username="plamenu_audio",
    actor__domain__name="funkwhale.local",
).first()

if existing is None:
    serializer = audio_serializers.ChannelCreateSerializer(
        data={
            "name": "Plamenu e2e audio",
            "username": "plamenu_audio",
            "description": {
                "text": "Audio publications for Plamenu federation tests",
                "content_type": "text/markdown",
            },
            "tags": ["plamenu", "e2e"],
            "content_category": "podcast",
            "metadata": {"language": "en", "itunes_category": "Sports"},
        },
        context={"actor": user.actor},
    )
    serializer.is_valid(raise_exception=True)
    existing = serializer.save(attributed_to=user.actor)

print(f"ready: @plamenu_audio@funkwhale.local (channel {existing.uuid})")
