# `x-s3s-payload-length`

`s3s` accepts an optional request header, `x-s3s-payload-length`, that declares how many bytes the sender delivers. The declaration travels with the request under the signature, so every hop can forward an exact length without buffering the body to measure it.

## Value

The value is the number of payload bytes after transfer decoding and before content decoding:

| Operation | Payload | Relation to the object length |
|---|---|---|
| `POST Object` | the data bytes of the file part | equal |
| `PUT Object` | the request body | equal |
| `UploadPart` | the bytes of that part | unrelated: the object length follows from the parts |

A value is a decimal integer without a sign, whitespace or a leading zero. The header may appear once; a request that repeats it, or carries a value that is not a decimal integer, is answered with `400 InvalidRequest`.

## Signing

The header carries the same weight as an `x-amz-*` header, because routing and input parsing read it. It must be covered by the signature:

- Header authentication and presigned URLs: the name has to appear in `SignedHeaders` (or `X-Amz-SignedHeaders`). A request that carries it unsigned is answered with `403 AccessDenied`, "There were headers present in the request which were not signed." A deployment that has to accept an unsigned `x-s3s-*` header can name it in `S3Config::unsigned_s3s_header_allowlist`; the list is empty by default.
- Presigned `POST` forms: the signature covers the policy document, not the headers, so the declaration is a **form field** that must also appear in the policy conditions. An HTTP header that does not go through the form and the policy counts as unsigned.

## Enforcement

A declaration never replaces a check, it adds one:

- On `POST Object` the declared value is enforced while the file part is read, together with the `content-length-range` policy condition and the configured maximum, whichever bound is tighter. The condition is still evaluated against the bytes actually delivered.
- On the other body-carrying operations the value must agree with the length the framing carries: fewer bytes are answered with `400 EntityTooSmall`, more with `400 EntityTooLarge`, and a request whose framing declares no length at all (a chunked body) is answered with `400 InvalidRequest` rather than accepted.
- No failure delivers an object: a request that disagrees with its declaration is rejected while it is read.

## Compatibility

The header is optional. A client that does not send it sees no behaviour change at all, and an implementation that does not know it is free to ignore it — the request still works, it only loses the ability to frame the body without measuring it. Nothing about the stored bytes, the routing or the authorization depends on the header.