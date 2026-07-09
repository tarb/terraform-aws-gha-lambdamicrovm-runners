###############################################################################
# Webhook ingress: a public Lambda Function URL (authType NONE). The aws provider
# auto-adds the public lambda:InvokeFunctionUrl permission on creation, so no
# separate aws_lambda_permission is needed. The endpoint is public - request
# authenticity relies entirely on the X-Hub-Signature-256 HMAC check that the
# dispatcher performs against the webhook secret.
###############################################################################

resource "aws_lambda_function_url" "webhook" {
  function_name      = aws_lambda_function.dispatcher.function_name
  authorization_type = "NONE"
  invoke_mode        = "BUFFERED" # one small buffered request/response
}
