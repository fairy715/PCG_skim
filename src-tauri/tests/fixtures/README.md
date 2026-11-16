# Test fixtures

| File | What it is | Where it came from |
|---|---|---|
| `sample.pst` | Four messages in Inbox and Projects: a Chinese subject with HTML and two attachments, a reply to it, plain text, HTML with an inline image. | Generated for this repository from `.eml` files with the `pstsmith` crate. |
| `outlook-dist-list.pst` | Written by Outlook. Its Contacts folder holds one contact and one distribution list of three members (the contact and two one-off addresses); its Calendar one appointment. | `src/test/resources/dist-list.pst` from [java-libpst](https://github.com/rjohnsondev/java-libpst), used under the Apache License 2.0 (`LICENSE.java-libpst.txt`). Not modified. |
