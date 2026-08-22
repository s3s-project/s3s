// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

/// <https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#access_policies-json>
pub(crate) fn example1_json() -> &'static str {
    r#"
    {
      "Version": "2012-10-17",
      "Statement": [
        {
          "Sid": "FirstStatement",
          "Effect": "Allow",
          "Action": ["iam:ChangePassword"],
          "Resource": "*"
        },
        {
          "Sid": "SecondStatement",
          "Effect": "Allow",
          "Action": "s3:ListAllMyBuckets",
          "Resource": "*"
        },
        {
          "Sid": "ThirdStatement",
          "Effect": "Allow",
          "Action": [
            "s3:List*",
            "s3:Get*"
          ],
          "Resource": [
            "arn:aws:s3:::confidential-data",
            "arn:aws:s3:::confidential-data/*"
          ],
          "Condition": {"Bool": {"aws:MultiFactorAuthPresent": "true"}}
        }
      ]
    }
    "#
}

/// <https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#access_policies-json>
pub(crate) fn example2_json() -> &'static str {
    r#"
    {
        "Version": "2012-10-17",
        "Statement": {
            "Effect": "Allow",
            "Action": "s3:ListBucket",
            "Resource": "arn:aws:s3:::example_bucket"
        }
    }    
    "#
}

/// <https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#access_policies-json>
pub(crate) fn example3_json() -> &'static str {
    r#"
    {
      "Version": "2012-10-17",
      "Statement": [{
        "Sid": "1",
        "Effect": "Allow",
        "Principal": {"AWS": ["arn:aws:iam::account-id:root"]},
        "Action": "s3:*",
        "Resource": [
          "arn:aws:s3:::mybucket",
          "arn:aws:s3:::mybucket/*"
        ]
      }]
    }
    "#
}
