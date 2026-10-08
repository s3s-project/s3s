# `x-s3s-payload-length`

`s3s` accepts an optional request header, `x-s3s-payload-length`, that declares how many bytes the sender delivers. The declaration travels with the request under the signature, so every hop can forward an exact length without buffering the body to measure it.

## Why this exists

A multipart form upload does not declare the length of the file it carries. `Content-Length` covers the whole form — the fields, the part headers and the closing boundary — and the size of the closing form depends on the client: a form that ends at the final boundary, one that adds a trailing CRLF and one that pads the epilogue produce different lengths for the same object. A `content-length-range` policy condition is a range, not a value, so it cannot recover the exact length either.

That leaves a service that has to hand the body to the next hop two options, and both cost something:

- **Measure it.** Read the whole file part before dispatching the operation. Correct, but it holds the body — memory linear in the upload size, which is exactly what a proxy in front of a storage backend cannot afford, and it is what a 256 MiB form upload showed: about 285 MiB of resident memory when the part was buffered against about 28 MiB when it was streamed.
- **Forward it without a length.** Framing that a backend does not accept is rejected rather than degraded: MinIO answers `411 Length Required` and Amazon S3 `501` for a body forwarded without a declared length, so the upload fails.

The header removes the choice. The sender declares the number of payload bytes once, the signature covers the declaration, and every hop can forward the body with an exact length without reading it first — while the receiver still counts the bytes as they arrive and rejects a declaration that disagrees. In a chain of `s3s` deployments (a gateway in front of a proxy in front of an origin) the length becomes ordinary request metadata instead of something to measure.

Two limits belong in the same breath:

- A backend that does not know the header cannot be helped by it: MinIO, Amazon S3 and every other implementation decide framing from what they actually receive. The header serves hops that speak it, and a hop that does not simply ignores it.
- The header declares framing, not content. It does not change what is stored, it is not a shortcut around authorization, and it never relaxes another check.

## Value

The value is the number of payload bytes after transfer decoding and before content decoding:

| Operation | Payload | Relation to the object length |
|---|---|---|
| `POST Object` | the data bytes of the file part | equal |
| `PUT Object` | the request body | equal |
| `UploadPart` | the bytes of that part | unrelated: the object length follows from the parts |

A value is a decimal integer without a sign, whitespace or a leading zero. The header may appear once; a request that repeats it, or carries a value that is not a decimal integer, is answered with `400 InvalidRequest`.

## ABNF

The field follows the HTTP grammar of RFC 9110 and the extension adds one rule. The value grammar applies to the field value after the optional whitespace a parser removes around it (RFC 9110 `field-value`), so `x-s3s-payload-length: 32` and `x-s3s-payload-length:32` are the same request.

```abnf
x-s3s-payload-length = payload-length
payload-length       = "0" / ( %x31-39 *DIGIT )

; %x31-39 is "1"-"9". A sign, a leading zero, a space inside the value and a value
; that does not fit in 64 bits are not payload-length: each is answered with
; 400 InvalidRequest.
```

- **Field name**: `x-s3s-payload-length`, matched case-insensitively, as HTTP field names are (RFC 9110 §5.1). It has to appear at most once — two values are ambiguous and ignoring them would drop a signed declaration — so a repeated field is `400 InvalidRequest` as well.
- **Form field** (a presigned `POST` form): the name is the literal `x-s3s-payload-length`, because a multipart field name is an opaque, exactly matched string, and the policy condition that covers it names the same field, for example `["eq", "$x-s3s-payload-length", "32"]`. A differently cased field is a field no condition covers, and a form field that no condition covers is rejected by the policy check.

## Signing

The header carries the same weight as an `x-amz-*` header, because routing and input parsing read it. It must be covered by the signature:

- Header authentication and presigned URLs: the name has to appear in `SignedHeaders` (or `X-Amz-SignedHeaders`). A request that carries it unsigned is answered with `403 AccessDenied`, "There were headers present in the request which were not signed." A deployment that has to accept an unsigned `x-s3s-*` header can name it in `S3Config::unsigned_s3s_header_allowlist`; the list is empty by default.
- Presigned `POST` forms: the signature covers the policy document, not the headers, so the declaration is a **form field** that must also appear in the policy conditions. An HTTP header that does not go through the form and the policy counts as unsigned.

## Enforcement

A declaration never replaces a check, it adds one:

- On `POST Object` the declared value is enforced while the file part is read, together with the `content-length-range` policy condition and the configured maximum, whichever bound is tighter. The condition is still evaluated against the bytes actually delivered.
- On the other body-carrying operations the value must agree with the length the framing carries: fewer bytes are answered with `400 EntityTooSmall`, more with `400 EntityTooLarge`, and a request whose framing declares no length at all is rejected rather than accepted: a deployment that requires a content length answers `411 MissingContentLength` (the default), and where that requirement is turned off, the declaration itself is answered with `400 InvalidRequest`.
- No failure delivers an object: a request that disagrees with its declaration is rejected while it is read.

## Relation to `x-amz-decoded-content-length`

The two headers are easy to confuse: both declare a number of payload bytes, and both are signed request metadata. They answer different questions.

`x-amz-decoded-content-length` belongs to one framing, an `aws-chunked` body. It is the length of the **whole request body** after that framing is removed, and it exists because the framing needs it: the chunk signatures are verified against it, and a mismatch fails while the body is decoded. That framing requires it, and without that framing it means nothing.

`x-s3s-payload-length` describes the payload **the operation delivers**, whatever the framing is: the file part on `POST Object`, the request body on `PUT Object`, the part on `UploadPart`. It is optional, it never drives a decoder, and it covers the case `x-amz-decoded-content-length` cannot: a form upload, whose request body is longer than the object and whose object length cannot be derived from `Content-Length`.

| | `x-amz-decoded-content-length` | `x-s3s-payload-length` |
|---|---|---|
| Declares | the whole body after `aws-chunked` decoding | the payload the operation delivers |
| Framing | only `Content-Encoding: aws-chunked`, which requires it | any body; optional in all of them |
| When absent | the framing cannot be decoded | nothing changes |
| Enforced by | the chunked decoder, while the body is read | the framing comparison, or the file-part range on `POST Object` |
| Signing weight | `x-amz-*`: it has to be in `SignedHeaders` | `x-s3s-*`: the same weight, with its own allowlist key |

The two can appear together, and on `PUT Object` they then describe the same count, because the decoded body length **is** the payload length. `s3s` replaces an `aws-chunked` body — and its `Content-Length` — with the decoded stream before the declaration is checked, so the comparison happens against the decoded length and the two agree by construction.

Reusing `x-amz-decoded-content-length` for form uploads was not an option: the name belongs to a framing a `POST` form does not use, and a hop that reads it as streaming framing would be misled by a value that is not the body length. A separate, ignorable declaration is the honest shape.

## Compatibility

The header is optional. A client that does not send it sees no behaviour change at all, and an implementation that does not know it is free to ignore it — the request still works, it only loses the ability to frame the body without measuring it. Nothing about the stored bytes, the routing or the authorization depends on the header.