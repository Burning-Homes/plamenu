"""Import the disposable fixture CA using the browser image's NSS ABI."""

import ctypes
import os
import ssl
from pathlib import Path


class Trust(ctypes.Structure):
    _fields_ = [
        ("ssl", ctypes.c_uint),
        ("email", ctypes.c_uint),
        ("object", ctypes.c_uint),
    ]


class Item(ctypes.Structure):
    _fields_ = [
        ("kind", ctypes.c_int),
        ("data", ctypes.c_void_p),
        ("length", ctypes.c_uint),
    ]


def main():
    if os.environ.get("A11Y_DISPOSABLE_TRUST") != "1":
        raise RuntimeError(
            "NSS trust import is only for the disposable browser fixture"
        )
    directory = Path("/tmp/.pki/nssdb")
    certificate = Path("/tmp/plamenu-fixture-ca.crt").read_bytes()
    # Existing libraries only: importing with host certutil can write trust
    # records unreadable by an older pinned browser image's NSS version.
    nss = ctypes.CDLL("libnss3.so")
    signatures = {
        "NSS_InitReadWrite": ([ctypes.c_char_p], ctypes.c_int),
        "NSS_Shutdown": ([], ctypes.c_int),
        "CERT_GetDefaultCertDB": ([], ctypes.c_void_p),
        "CERT_NewTempCertificate": (
            [
                ctypes.c_void_p,
                ctypes.POINTER(Item),
                ctypes.c_char_p,
                ctypes.c_int,
                ctypes.c_int,
            ],
            ctypes.c_void_p,
        ),
        "CERT_ChangeCertTrust": (
            [ctypes.c_void_p, ctypes.c_void_p, ctypes.POINTER(Trust)],
            ctypes.c_int,
        ),
        "CERT_GetCertTrust": ([ctypes.c_void_p, ctypes.POINTER(Trust)], ctypes.c_int),
        "CERT_DestroyCertificate": ([ctypes.c_void_p], None),
        "PK11_GetInternalKeySlot": ([], ctypes.c_void_p),
        "PK11_NeedUserInit": ([ctypes.c_void_p], ctypes.c_int),
        "PK11_InitPin": (
            [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p],
            ctypes.c_int,
        ),
        "PK11_ImportCert": (
            [
                ctypes.c_void_p,
                ctypes.c_void_p,
                ctypes.c_ulong,
                ctypes.c_char_p,
                ctypes.c_int,
            ],
            ctypes.c_int,
        ),
        "PK11_FreeSlot": ([ctypes.c_void_p], None),
    }
    for name, (arguments, result) in signatures.items():
        function = getattr(nss, name)
        function.argtypes, function.restype = arguments, result

    def check(status):
        if status != 0:
            raise RuntimeError("Disposable NSS certificate import failed")

    check(nss.NSS_InitReadWrite(f"sql:{directory}".encode()))
    cert = slot = None
    try:
        der = ssl.PEM_cert_to_DER_cert(certificate.decode("ascii"))
        buffer = ctypes.create_string_buffer(der)
        item = Item(0, ctypes.cast(buffer, ctypes.c_void_p), len(der))
        cert = nss.CERT_NewTempCertificate(
            nss.CERT_GetDefaultCertDB(), ctypes.byref(item), None, 0, 1
        )
        slot = nss.PK11_GetInternalKeySlot()
        if not cert or not slot:
            raise RuntimeError("Disposable NSS certificate or slot unavailable")
        if nss.PK11_NeedUserInit(slot):
            check(nss.PK11_InitPin(slot, None, b""))
        check(nss.PK11_ImportCert(slot, cert, 0, b"Plamenu disposable fixture", 1))
        # NSS CERTDB_VALID_CA | CERTDB_TRUSTED_CA: TLS server CA trust only.
        trust = Trust(0x08 | 0x10, 0, 0)
        check(
            nss.CERT_ChangeCertTrust(
                nss.CERT_GetDefaultCertDB(), cert, ctypes.byref(trust)
            )
        )
        actual = Trust()
        check(nss.CERT_GetCertTrust(cert, ctypes.byref(actual)))
        if actual.ssl & 0x18 != 0x18 or actual.email or actual.object:
            raise RuntimeError("Disposable NSS trust verification failed")
    finally:
        if cert:
            nss.CERT_DestroyCertificate(cert)
        if slot:
            nss.PK11_FreeSlot(slot)
        check(nss.NSS_Shutdown())


if __name__ == "__main__":
    main()
