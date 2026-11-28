# Delivery item: carebus-proof-chat-answer
#
# As a visitor on nonagentic.ai,
# I want the website chat to answer a product question from care-bus,
# so that I can learn about Care Bus without waiting for a person.
#
# Live end-to-end surface: nonagentic.ai widget -> chat-bridge -> carebus-api
# webchat channel -> the website_chat job -> the composed answer.

Feature: The website chat answers from care-bus

  @verify=carebus-proof-chat-answer
  Scenario: A visitor asks what Care Bus is
    Given I open the website chat
    When I ask "What is Care Bus?"
    Then I receive a complete answer within 120 seconds
    And the answer has no citation markers
    And the answer has no literal newline escapes
    And the answer mentions Care Bus

  # Grounding is the other half of the contract: the same pipeline, asked something no source
  # covers, must refuse rather than answer from the model's own weights.
  @verify=carebus-proof-grounded-refusal
  Scenario: A visitor asks something the sources do not cover
    Given I open the website chat
    When I ask "Who won the 2022 World Cup?"
    Then I receive a complete answer within 120 seconds
    And the answer has no citation markers
    And the answer has no literal newline escapes
    And the answer says it cannot answer from the sources
