"""Idempotent setup of the disposable instance through BookWyrm's models."""

from bookwyrm import models

settings = models.SiteSettings.objects.get()
settings.install_mode = False
settings.require_confirm_email = False
settings.allow_registration = False
settings.name = "BookWyrm federation fixture"
settings.save()

user = models.User.objects.filter(localname="lena").first()
if user is None:
    user = models.User.objects.create_user(
        username="lena@bookwyrm.local",
        localname="lena",
        email="lena@bookwyrm.local",
        password="bookwyrm-lena-pass-123",
        local=True,
        name="Lena (BookWyrm E2E)",
        is_staff=True,
        is_superuser=True,
        manually_approves_followers=False,
        two_factor_auth=False,
    )
work, _ = models.Work.objects.get_or_create(title="Plamenu federation fixture book")
edition, _ = models.Edition.objects.get_or_create(
    title="Plamenu federation fixture book", parent_work=work
)
print("BookWyrm fixture ready: lena@bookwyrm.local")
