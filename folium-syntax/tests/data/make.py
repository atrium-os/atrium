#!/usr/bin/env python3
"""Make the encrypted fixtures with pypdf (an independent writer).
Run from this folder: python3 make.py. Each file has one page whose
content stream is `BT /F1 24 Tf 72 700 Td (Folium secret) Tj ET` and an
/Info /Title of "Folium title"."""
from pypdf import PdfWriter
from pypdf.generic import ContentStream, NameObject, DecodedStreamObject

CONTENT = b"BT /F1 24 Tf 72 700 Td (Folium secret) Tj ET"

def base():
    w = PdfWriter()
    p = w.add_blank_page(612, 792)
    s = DecodedStreamObject()
    s.set_data(CONTENT)
    p[NameObject("/Contents")] = w._add_object(s)
    w.add_metadata({"/Title": "Folium title"})
    return w

for name, alg, user in [
    ("rc4-40", "RC4-40", ""),
    ("rc4-128", "RC4-128", ""),
    ("aes-128", "AES-128", ""),
    ("aes-256", "AES-256", ""),
    ("aes-256-user", "AES-256", "secret"),
    ("rc4-128-user", "RC4-128", "secret"),
]:
    w = base()
    w.encrypt(user_password=user, owner_password="owner", algorithm=alg)
    with open(f"enc-{name}.pdf", "wb") as f:
        w.write(f)
