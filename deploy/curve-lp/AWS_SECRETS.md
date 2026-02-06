# AWS Secrets Management

For production on AWS, don't use `.env` files. Use proper secrets management instead.

## Option 1: AWS Secrets Manager (Recommended)

### Store secrets:
```bash
# Store RPC URL
aws secretsmanager create-secret \
  --name curve-lp-solver/node-url \
  --secret-string "https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"

# Store private key
aws secretsmanager create-secret \
  --name curve-lp-solver/solver-account \
  --secret-string "0xYOUR_PRIVATE_KEY"
```

### Retrieve at runtime:
```bash
# In your startup script or ECS task definition
export NODE_URL=$(aws secretsmanager get-secret-value \
  --secret-id curve-lp-solver/node-url \
  --query SecretString --output text)

export SOLVER_ACCOUNT=$(aws secretsmanager get-secret-value \
  --secret-id curve-lp-solver/solver-account \
  --query SecretString --output text)
```

### Cost: ~$0.40/secret/month + $0.05 per 10,000 API calls

---

## Option 2: AWS SSM Parameter Store (Free tier available)

### Store secrets:
```bash
# Store as SecureString (encrypted)
aws ssm put-parameter \
  --name "/curve-lp-solver/node-url" \
  --type "SecureString" \
  --value "https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"

aws ssm put-parameter \
  --name "/curve-lp-solver/solver-account" \
  --type "SecureString" \
  --value "0xYOUR_PRIVATE_KEY"
```

### Retrieve at runtime:
```bash
export NODE_URL=$(aws ssm get-parameter \
  --name "/curve-lp-solver/node-url" \
  --with-decryption \
  --query Parameter.Value --output text)

export SOLVER_ACCOUNT=$(aws ssm get-parameter \
  --name "/curve-lp-solver/solver-account" \
  --with-decryption \
  --query Parameter.Value --output text)
```

### Cost: Free for standard parameters, $0.05 per 10,000 API calls for advanced

---

## Option 3: ECS Task Definition with Secrets

If using ECS, reference secrets directly in task definition:

```json
{
  "containerDefinitions": [
    {
      "name": "driver",
      "secrets": [
        {
          "name": "NODE_URL",
          "valueFrom": "arn:aws:secretsmanager:us-east-1:123456789:secret:curve-lp-solver/node-url"
        },
        {
          "name": "SOLVER_ACCOUNT",
          "valueFrom": "arn:aws:secretsmanager:us-east-1:123456789:secret:curve-lp-solver/solver-account"
        }
      ]
    }
  ]
}
```

---

## IAM Policy Required

Your EC2 instance or ECS task needs this IAM policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "secretsmanager:GetSecretValue"
      ],
      "Resource": [
        "arn:aws:secretsmanager:*:*:secret:curve-lp-solver/*"
      ]
    }
  ]
}
```

Or for SSM:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ssm:GetParameter",
        "ssm:GetParameters"
      ],
      "Resource": [
        "arn:aws:ssm:*:*:parameter/curve-lp-solver/*"
      ]
    },
    {
      "Effect": "Allow",
      "Action": [
        "kms:Decrypt"
      ],
      "Resource": [
        "arn:aws:kms:*:*:key/*"
      ]
    }
  ]
}
```

---

## Modified deploy.sh for AWS

```bash
#!/bin/bash
set -euo pipefail

# Fetch secrets from AWS
export NODE_URL=$(aws secretsmanager get-secret-value \
  --secret-id curve-lp-solver/node-url \
  --query SecretString --output text)

export SOLVER_ACCOUNT=$(aws secretsmanager get-secret-value \
  --secret-id curve-lp-solver/solver-account \
  --query SecretString --output text)

# Process configs and start
envsubst < driver.toml > ./processed/driver.toml
envsubst < curve-lp.prod.toml > ./processed/curve-lp.toml

docker-compose -f docker-compose.prod.yml up -d
```

---

## Security Best Practices

1. **Rotate keys regularly** - Set up automatic rotation in Secrets Manager
2. **Least privilege IAM** - Only allow access to specific secrets
3. **Audit access** - Enable CloudTrail logging for secret access
4. **Don't log secrets** - Ensure your app doesn't log NODE_URL or SOLVER_ACCOUNT
5. **Use VPC endpoints** - Access Secrets Manager without internet if in VPC
